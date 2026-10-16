use super::*;
use crate::memory::experience::model::{ExperienceStatus, ExperienceType};
use crate::memory::experience::store::ListFilter;
use crate::memory::experience::vector::{EmbeddingProvider, HashEmbedding};
use serde_json::json;

const GOAL: &str = "tell me today stock market condition";

fn svc(name: &str) -> Arc<LearningService> {
    let d = std::env::temp_dir().join(format!("aqua_agentexp_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let s = LearningService::manual(&d, Arc::new(HashEmbedding));
    let mut st = s.settings();
    st.similarity_threshold = 0.3;
    s.update_settings(&st).unwrap();
    s
}

fn tools() -> Vec<McpTool> {
    ["browser_navigate", "browser_snapshot"]
        .iter()
        .map(|n| McpTool { name: n.to_string(), description: String::new(), input_schema: json!({}), source: Some("browser".into()) })
        .collect()
}

fn tool_step(tool: &str, args: Value, obs: &str) -> AgentStep {
    AgentStep { step: 1, kind: "tool".into(), detail: json!({ "tool": tool, "arguments": args, "observation": obs }) }
}

const PAGE: &str = "Page URL: https://www.google.com/search?q=stock%20market%20condition%20today\nPage content:\nNIFTY 50 22,716.20 (-64.05) -0.28% SENSEX 72,529.07 (-242.65) -0.33%";

fn run_result(stopped: &str, answer: &str) -> McpAgentResult {
    McpAgentResult {
        final_answer: answer.into(),
        steps: vec![
            AgentStep { step: 1, kind: "llm".into(), detail: json!({ "raw": "{\"tool\":\"browser_snapshot\"} the planner's own words" }) },
            tool_step("browser_navigate", json!({ "url": "https://www.google.com/search?q=stock+market+condition+today" }), PAGE),
            tool_step("browser_snapshot", json!({}), PAGE),
        ],
        tool_rounds: 2,
        stopped_reason: stopped.into(),
        gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
    }
}

fn good() -> McpAgentResult {
    run_result("final", "The Nifty 50 is at 22,716.20, down 64.05 points (-0.28%), and the Sensex is at 72,529.07.")
}

fn invented() -> McpAgentResult {
    run_result("final", "The Sensex is at 99,999.99 and the Nifty 50 is at 44,444.44.")
}

fn drain(s: &LearningService) {
    let mut now = now_secs();
    for _ in 0..10 {
        if !s.process_one(now).unwrap() {
            now += 3600;
            if !s.process_one(now).unwrap() {
                break;
            }
        }
    }
}

fn experiences(s: &LearningService) -> Vec<crate::memory::experience::model::Experience> {
    s.list(&ListFilter::default()).unwrap()
}

#[test]
fn evidence_decides_success_not_the_models_word() {
    let ok = evaluate_run(GOAL, &good());
    assert!(ok.verified && ok.method == VerificationMethod::PageEvidence, "{ok:?}");
    let fake = evaluate_run(GOAL, &invented());
    assert!(!fake.verified && fake.result.contains("not found in what was read"), "{fake:?}");
    let ran_out = evaluate_run(GOAL, &run_result("max_steps", "some text"));
    assert!(!ran_out.verified && ran_out.result.contains("without a finished answer"));
    // A cap of the autonomy setting ended the run: whatever it then said, it did not finish, even when the answer repeats numbers that were read.
    let capped = evaluate_run(GOAL, &run_result("limit", "I stopped because this run reached its limit of tool calls (200). The Nifty 50 is at 22,716.20."));
    assert!(!capped.verified && capped.result.contains("(limit) without a finished answer"), "{capped:?}");
    let chat = McpAgentResult { final_answer: "hello".into(), steps: vec![], tool_rounds: 0, stopped_reason: "final".into(), gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new() };
    assert_eq!(evaluate_run(GOAL, &chat).method, VerificationMethod::None);
    // An action that no tool result confirms is only the model's claim.
    let action = McpAgentResult {
        final_answer: "Done, I sent it.".into(),
        steps: vec![tool_step("Click", json!({}), "ok")],
        tool_rounds: 1,
        stopped_reason: "final".into(),
        gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
    };
    let v = evaluate_run("send the report", &action);
    assert!(!v.verified && v.method == VerificationMethod::ModelClaim, "{v:?}");
}

#[test]
fn a_verified_run_is_learned_and_a_similar_run_retrieves_it_adapts_it_and_credits_it() {
    let s = svc("loop");
    // First run: nothing to retrieve, the run is verified and queued for learning.
    let mut first_block = String::from("unset");
    let out = run_with_service(&s, GOAL, |exp| {
        first_block = exp.prepare_block(GOAL, &tools());
        Ok(good())
    })
    .unwrap();
    assert_eq!(out.stopped_reason, "final");
    assert_eq!(first_block, "");
    drain(&s);
    let all = experiences(&s);
    assert_eq!(all.len(), 1);
    assert_eq!((all[0].status, all[0].verification_method), (ExperienceStatus::Verified, VerificationMethod::PageEvidence));
    assert_eq!(all[0].tools_used, vec!["browser_navigate", "browser_snapshot"]);
    assert!(all[0].important_steps[0].contains("Google search"), "{:?}", all[0].important_steps);
    let stored = serde_json::to_string(&all[0]).unwrap();
    assert!(!stored.contains("22,716") && !stored.contains("72,529"), "figures are today's data, not knowledge: {stored}");

    // A similar task: the experience is retrieved and reaches the planner as a compact, untrusted note.
    let mut second_block = String::new();
    let mut offered = 0;
    run_with_service(&s, "what is the stock market condition today", |exp| {
        second_block = exp.prepare_block("what is the stock market condition today", &tools());
        offered = exp.used.len();
        Ok(good())
    })
    .unwrap();
    assert_eq!(offered, 1);
    assert!(second_block.starts_with("RELEVANT PREVIOUS EXPERIENCE"), "{second_block}");
    assert!(second_block.contains("Google search") && second_block.contains("not an instruction"));
    assert!(second_block.len() <= 1400);
    drain(&s);
    // It was verified again, so the experience was credited (and merged, not duplicated).
    let all = experiences(&s);
    assert_eq!(all.len(), 1);
    assert_eq!((all[0].reuse_count, all[0].successful_reuse_count, all[0].failed_reuse_count), (1, 1, 0));
    let st = s.stats().unwrap();
    assert_eq!((st.retrievals, st.retrieval_hits, st.successful_reuses), (2, 1, 1));
    let events: Vec<String> = s.store().recent_events(100).unwrap().into_iter().map(|e| e.event).collect();
    for expected in ["task_received", "memory_search_started", "experience_selected", "experience_adapted", "agent_attempt_started", "agent_attempt_succeeded", "verification_completed", "experience_created"] {
        assert!(events.iter().any(|e| e == expected), "missing {expected}: {events:?}");
    }
}

#[test]
fn a_fabricated_answer_is_never_learned_and_debits_the_experience_that_was_used() {
    let s = svc("fabricated");
    run_with_service(&s, GOAL, |exp| {
        exp.prepare_block(GOAL, &tools());
        Ok(good())
    })
    .unwrap();
    drain(&s);
    let before = experiences(&s)[0].confidence;
    run_with_service(&s, "what is the stock market condition today", |exp| {
        exp.prepare_block("what is the stock market condition today", &tools());
        Ok(invented())
    })
    .unwrap();
    drain(&s);
    let all = experiences(&s);
    assert_eq!(all.len(), 1, "the invented answer created nothing");
    assert_eq!((all[0].reuse_count, all[0].failed_reuse_count), (1, 1));
    assert!(all[0].confidence < before, "using it led to a wrong answer: less trust");
    // A run that ran out of steps is not learned either.
    run_with_service(&s, "read the weather forecast for pune today", |exp| {
        exp.prepare_block("read the weather forecast for pune today", &tools());
        Ok(run_result("max_steps", "I could not finish"))
    })
    .unwrap();
    drain(&s);
    assert_eq!(experiences(&s).len(), 1);
}

#[test]
fn experimentation_is_off_by_default_so_a_failed_run_is_not_retried() {
    let s = svc("noexp");
    let mut calls = 0;
    let out = run_with_service(&s, GOAL, |_| {
        calls += 1;
        Ok(invented())
    })
    .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(out.final_answer, invented().final_answer, "the result is passed through untouched");
}

#[test]
fn bounded_experimentation_retries_with_lessons_then_stops_and_says_so() {
    let s = svc("exp");
    let mut st = s.settings();
    st.autonomous_experimentation = true;
    st.max_attempts = 3;
    s.update_settings(&st).unwrap();
    let mut calls = 0;
    let mut lessons: Vec<String> = Vec::new();
    let out = run_with_service(&s, GOAL, |exp| {
        calls += 1;
        lessons.push(exp.extra_lessons.clone());
        Ok(invented())
    })
    .unwrap();
    assert_eq!(calls, 3, "exactly the configured number of attempts");
    assert_eq!(lessons[0], "");
    assert!(lessons[1].contains("EARLIER ATTEMPTS OF THIS TASK FAILED") && lessons[1].contains("not found in what was read"), "{}", lessons[1]);
    assert!(lessons[2].matches("Tried:").count() == 2);
    assert_eq!(out.stopped_reason, "experiment_limit");
    assert!(out.final_answer.contains("could not reliably complete this") && out.final_answer.contains("maximum number of attempts") && out.final_answer.contains("3 attempt"), "{}", out.final_answer);
    assert!(!out.final_answer.contains("99,999"), "the unverified guess is not returned");
    assert_eq!(experiences(&s).len(), 0);
    assert!(s.store().recent_events(100).unwrap().iter().any(|e| e.event == "agent_usage"));
}

#[test]
fn a_stop_on_purpose_that_needs_the_person_is_not_retried_and_its_own_answer_is_kept() {
    for reason in ["needs_login", "user_declined", "policy_denied", "voice_cannot_act", "browser_unavailable", "waiting_user", "limit"] {
        let s = svc(&format!("deliberate_{reason}"));
        let mut st = s.settings();
        st.autonomous_experimentation = true;
        st.max_attempts = 3;
        s.update_settings(&st).unwrap();
        let mut calls = 0;
        let out = run_with_service(&s, GOAL, |_| {
            calls += 1;
            Ok(run_result(reason, "Facebook is asking you to sign in, so I stopped."))
        })
        .unwrap();
        assert_eq!(calls, 1, "{reason}: one attempt, not three");
        assert_eq!((out.stopped_reason.as_str(), out.final_answer.as_str()), (reason, "Facebook is asking you to sign in, so I stopped."), "{reason}");
    }
}

#[test]
fn a_model_context_error_is_not_retried() {
    let s = svc("ctx");
    let mut st = s.settings();
    st.autonomous_experimentation = true;
    st.max_attempts = 3;
    s.update_settings(&st).unwrap();
    let mut calls = 0;
    let out = run_with_service(&s, GOAL, |_| {
        calls += 1;
        Ok(run_result(
            "llm_error: the language model refused this request (HTTP 400: exceeds the available context size (2048 tokens))",
            "",
        ))
    })
    .unwrap();
    assert_eq!(calls, 1);
    assert!(out.stopped_reason.starts_with("llm_error"));
    assert!(!out.final_answer.contains("maximum number of attempts"));
}

#[test]
fn a_run_that_used_its_steps_is_not_started_again() {
    let s = svc("steps");
    let mut st = s.settings();
    st.autonomous_experimentation = true;
    st.max_attempts = 3;
    s.update_settings(&st).unwrap();
    let mut calls = 0;
    let out = run_with_service(&s, GOAL, |_| {
        calls += 1;
        Ok(run_result("max_steps", "Regarding the question: Markets fall after the rate decision today"))
    })
    .unwrap();
    assert_eq!(calls, 1);
    assert!(out.final_answer.contains("Markets fall"), "{out:?}");
    assert!(!out.final_answer.contains("token budget"), "{out:?}");
}

#[test]
fn opening_an_app_directly_is_not_retried_or_replaced() {
    let s = svc("thispc");
    let mut st = s.settings();
    st.autonomous_experimentation = true;
    st.max_attempts = 3;
    s.update_settings(&st).unwrap();
    let mut calls = 0;
    let out = run_with_service(&s, "open this pc", |_| {
        calls += 1;
        Ok(McpAgentResult {
            final_answer: "Opened This PC.".into(),
            steps: vec![],
            tool_rounds: 0,
            stopped_reason: "scripted_open_app".into(),
            gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
        })
    })
    .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(out.final_answer, "Opened This PC.");
    assert_eq!(out.stopped_reason, "scripted_open_app");
    let v = evaluate_run("open this pc", &out);
    assert!(v.verified, "{v:?}");
}

#[test]
fn opening_a_site_is_not_started_again() {
    let s = svc("edgefb");
    let mut st = s.settings();
    st.autonomous_experimentation = true;
    st.max_attempts = 3;
    s.update_settings(&st).unwrap();
    let mut calls = 0;
    let out = run_with_service(&s, "in edge open facebook", |_| {
        calls += 1;
        Ok(McpAgentResult {
            final_answer: "Opened Facebook in Edge.".into(),
            steps: vec![tool_step("browser_navigate", json!({ "url": "https://www.facebook.com" }), "Navigated to https://www.facebook.com")],
            tool_rounds: 1,
            stopped_reason: "final".into(),
            gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
        })
    })
    .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(out.final_answer, "Opened Facebook in Edge.");
    assert!(!out.final_answer.contains("maximum number of attempts"));
}

#[test]
fn a_correction_found_on_a_later_attempt_is_learned_as_an_error_fix() {
    let s = svc("fix");
    let mut st = s.settings();
    st.autonomous_experimentation = true;
    s.update_settings(&st).unwrap();
    let mut calls = 0;
    let out = run_with_service(&s, GOAL, |_| {
        calls += 1;
        Ok(if calls == 1 { invented() } else { good() })
    })
    .unwrap();
    assert_eq!(calls, 2);
    assert!(out.final_answer.contains("22,716.20"));
    drain(&s);
    let all = experiences(&s);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].experience_type, ExperienceType::ErrorFix);
    assert!(all[0].failure_reason.as_deref().unwrap().contains("not found in what was read"));
    assert!(all[0].correction.is_some() && all[0].failed_approach.is_some());
}

#[test]
fn the_execution_time_limit_stops_experimentation() {
    let s = svc("time");
    let mut st = s.settings();
    st.autonomous_experimentation = true;
    st.max_attempts = 10;
    s.update_settings(&st).unwrap();
    // Force the tool-call ceiling instead of waiting on the clock: 24 calls allowed, 12 used per run.
    let mut calls = 0;
    let out = run_with_service(&s, GOAL, |_| {
        calls += 1;
        let mut r = invented();
        r.steps = (0..12).map(|_| tool_step("browser_snapshot", json!({}), PAGE)).collect();
        Ok(r)
    })
    .unwrap();
    assert_eq!(calls, 2, "stops when the tool-call budget is used up");
    assert!(out.final_answer.contains("maximum number of tool calls"), "{}", out.final_answer);
}

#[test]
fn learning_off_leaves_the_run_untouched_and_queues_nothing() {
    let s = svc("off");
    let mut st = s.settings();
    st.learning_enabled = false;
    s.update_settings(&st).unwrap();
    let out = run_with_service(&s, GOAL, |exp| {
        exp.prepare_block(GOAL, &tools());
        Ok(good())
    })
    .unwrap();
    assert_eq!(out.final_answer, good().final_answer);
    drain(&s);
    assert_eq!(experiences(&s).len(), 0);
    assert!(s.recent_jobs(10).unwrap().is_empty());
}

#[test]
fn an_error_from_the_agent_passes_through_and_learns_nothing() {
    let s = svc("err");
    let err = run_with_service(&s, GOAL, |_| Err(anyhow::anyhow!("browser unavailable"))).unwrap_err();
    assert!(err.to_string().contains("browser unavailable"));
    assert!(s.recent_jobs(10).unwrap().is_empty());
}

#[test]
fn a_broken_memory_does_not_stop_the_agent() {
    struct Down;
    impl EmbeddingProvider for Down {
        fn model(&self) -> &str { "down" }
        fn version(&self) -> u32 { 1 }
        fn dim(&self) -> usize { 384 }
        fn embed(&self, _: &str) -> Result<Vec<f32>, String> { Err("service down".into()) }
    }
    let d = std::env::temp_dir().join(format!("aqua_agentexp_down_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let s = LearningService::manual(&d, Arc::new(Down));
    let mut block = String::from("unset");
    let out = run_with_service(&s, GOAL, |exp| {
        block = exp.prepare_block(GOAL, &tools());
        Ok(good())
    })
    .unwrap();
    assert_eq!(block, "");
    assert_eq!(out.final_answer, good().final_answer, "the user's answer is unaffected");
    drain(&s);
    assert_eq!(s.status().jobs_dead, 1, "learning failed quietly and was parked");
}

#[test]
fn step_descriptions_carry_no_search_terms_typed_text_or_credentials() {
    assert_eq!(step_line("browser_navigate", &json!({ "url": "https://www.google.com/search?q=my+secret+diagnosis" })), "browser_navigate to a Google search for the topic");
    assert_eq!(step_line("browser_navigate", &json!({ "url": "https://user:pw123@portal.example.com/a/b?token=abc" })), "browser_navigate to portal.example.com");
    let typed = step_line("browser_type", &json!({ "text": "hunter2", "ref": "e5" }));
    assert!(!typed.contains("hunter2"), "{typed}");
    let other = step_line("db.query", &json!({ "sql": "select * from users", "database": "x" }));
    assert!(!other.contains("select") && other.contains("sql"), "{other}");
}

#[test]
fn the_evaluation_compares_runs_with_and_without_memory() {
    let s = svc("eval");
    // Two runs without memory (one verified, one not), then two with memory (both verified).
    run_with_service(&s, GOAL, |exp| { exp.prepare_block(GOAL, &tools()); Ok(good()) }).unwrap();
    run_with_service(&s, "read the weather forecast for pune today", |_| Ok(invented())).unwrap();
    drain(&s);
    for _ in 0..2 {
        run_with_service(&s, "what is the stock market condition today", |exp| { exp.prepare_block("what is the stock market condition today", &tools()); Ok(good()) }).unwrap();
    }
    let ev = s.store().evaluation().unwrap();
    assert_eq!((ev.runs_without_memory, ev.runs_with_memory), (2, 2));
    assert_eq!((ev.success_rate_without_memory, ev.success_rate_with_memory), (0.5, 1.0));
    assert_eq!(ev.memory_related_failures, 0);
    assert!(ev.avg_tool_calls_with_memory > 0.0 && ev.avg_tokens_with_memory > 0);
    assert!(ev.note.contains("Fewer than 10 runs"), "small samples are labelled as such: {}", ev.note);
}

#[test]
fn a_cancelled_run_neither_learns_nor_debits_the_experience_it_used() {
    let s = svc("cancel");
    run_with_service(&s, GOAL, |exp| {
        exp.prepare_block(GOAL, &tools());
        Ok(good())
    })
    .unwrap();
    drain(&s);
    let before = experiences(&s)[0].clone();
    let jobs_before = s.recent_jobs(50).unwrap().len();
    // The user cancels (or the client goes away) partway through a similar task.
    run_with_service(&s, "what is the stock market condition today", |exp| {
        exp.prepare_block("what is the stock market condition today", &tools());
        Ok(McpAgentResult { final_answer: "Stopped.".into(), steps: vec![], tool_rounds: 0, stopped_reason: "cancelled".into(), gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new() })
    })
    .unwrap();
    drain(&s);
    let after = experiences(&s)[0].clone();
    assert_eq!((after.reuse_count, after.failed_reuse_count, after.confidence), (before.reuse_count, 0, before.confidence), "a cancelled run says nothing about whether the experience helped");
    assert_eq!(s.recent_jobs(50).unwrap().len(), jobs_before, "nothing was queued for learning");
    assert!(s.store().recent_events(50).unwrap().iter().any(|e| e.event == "learning_skipped"));
}

#[test]
fn a_practice_run_is_unattended_single_attempt_and_not_a_user_task() {
    let s = svc("practice");
    let mut st = s.settings();
    st.autonomous_experimentation = true; // retries are for a person's task, never for practice
    st.max_attempts = 3;
    s.update_settings(&st).unwrap();
    assert!(!is_practice() && !aqua_mcp_adapters::browser::is_unattended());
    let activity_before = s.activity().last();
    let mut calls = 0;
    let mut was_unattended = false;
    {
        let _scope = practice_scope();
        assert!(is_practice() && aqua_mcp_adapters::browser::is_unattended(), "practice runs are unattended: approvals are refused at once");
        let out = run_with_service(&s, GOAL, |exp| {
            calls += 1;
            was_unattended = aqua_mcp_adapters::browser::is_unattended();
            exp.prepare_block(GOAL, &tools());
            Ok(invented())
        })
        .unwrap();
        assert_eq!(out.final_answer, invented().final_answer, "untouched: no experiment-limit message either");
        assert!(!s.activity().is_active(), "a practice run is not a user's task");
    }
    assert_eq!(calls, 1, "one attempt even with experimentation on");
    assert!(was_unattended);
    assert!(!is_practice() && !aqua_mcp_adapters::browser::is_unattended(), "the scope ends with the guard");
    assert_eq!(s.activity().last(), activity_before, "practice does not count as a user using the agent");
    assert_eq!(s.store().evaluation().unwrap().runs_without_memory + s.store().evaluation().unwrap().runs_with_memory, 0, "and it stays out of the with/without-memory comparison");
    // A verified practice run is learned, with a task id that shows where it came from.
    {
        let _scope = practice_scope();
        run_with_service(&s, GOAL, |exp| {
            exp.prepare_block(GOAL, &tools());
            Ok(good())
        })
        .unwrap();
    }
    drain(&s);
    assert_eq!(experiences(&s).len(), 1);
    assert!(experiences(&s)[0].source_task_id.starts_with("practice-run-"), "{}", experiences(&s)[0].source_task_id);
}

#[test]
fn a_users_task_marks_the_agent_as_active_and_then_recently_used() {
    let s = svc("active");
    assert!(!s.activity().is_active() && s.activity().last() == 0);
    let mut during = false;
    let before = now_secs();
    run_with_service(&s, GOAL, |_| {
        during = s.activity().is_active();
        Ok(good())
    })
    .unwrap();
    assert!(during, "practice would wait while this runs");
    assert!(!s.activity().is_active());
    assert!(s.activity().last() >= before, "and for a while after it");
}

// ---- coding tasks: judged by the tests, build and linter, not by the model's word ----------------

const CODING_GOAL: &str = "fix the failing test in the order parser";
const CARGO_PASS: &str = "exit code 0\n--- stdout ---\nrunning 14 tests\ntest result: ok. 14 passed; 0 failed; 0 ignored\n";
const CARGO_FAIL_OUT: &str = "exit code 101\n--- stdout ---\nrunning 14 tests\ntest result: FAILED. 13 passed; 1 failed; 0 ignored\n";

fn coding_tools() -> Vec<McpTool> {
    ["fs.read_file", "fs.edit", "term.run"]
        .iter()
        .map(|n| McpTool { name: n.to_string(), description: String::new(), input_schema: json!({}), source: Some("host".into()) })
        .collect()
}

fn coding_run(answer: &str, calls: Vec<(&str, Value, &str)>) -> McpAgentResult {
    McpAgentResult {
        final_answer: answer.into(),
        steps: calls.into_iter().map(|(tool, args, obs)| tool_step(tool, args, obs)).collect(),
        tool_rounds: 3,
        stopped_reason: "final".into(),
        gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
    }
}

fn cargo_test(obs: &'static str) -> (&'static str, Value, &'static str) {
    ("term.run", json!({ "exe": "cargo", "args": ["test", "--lib"] }), obs)
}

fn read_and_edit() -> Vec<(&'static str, Value, &'static str)> {
    vec![("fs.read_file", json!({ "path": "src/parser.rs" }), "fn parse() {}"), ("fs.edit", json!({ "path": "src/parser.rs" }), "edited 1 place")]
}

#[test]
fn a_coding_task_is_verified_by_its_tests_and_a_failing_run_counts_against_it_whatever_the_model_says() {
    let mut calls = read_and_edit();
    calls.push(cargo_test(CARGO_PASS));
    let ok = evaluate_run(CODING_GOAL, &coding_run("Fixed the parser; all tests pass.", calls));
    assert!(ok.verified && ok.method == VerificationMethod::Tests, "{ok:?}");
    assert!(ok.result.contains("14 passed"), "{}", ok.result);

    // The model says it is done, the tests say otherwise: a failure, by the strongest method.
    let mut calls = read_and_edit();
    calls.push(cargo_test(CARGO_FAIL_OUT));
    let bad = evaluate_run(CODING_GOAL, &coding_run("All done, the implementation is complete.", calls));
    assert!(!bad.verified && bad.method == VerificationMethod::Tests, "{bad:?}");
    assert!(bad.result.contains("failed"), "{}", bad.result);

    // Tests passed, then the code was edited again: nothing shows it still passes (unknown, not a failure).
    let mut calls = read_and_edit();
    calls.push(cargo_test(CARGO_PASS));
    calls.push(("fs.edit", json!({ "path": "src/parser.rs" }), "edited 1 place"));
    let stale = evaluate_run(CODING_GOAL, &coding_run("Done.", calls));
    assert!(!stale.verified && stale.method == VerificationMethod::ModelClaim && stale.result.contains("not run again"), "{stale:?}");

    // No check at all: still only the model's word.
    let none = evaluate_run(CODING_GOAL, &coding_run("Done, I fixed it.", read_and_edit()));
    assert!(!none.verified && none.method == VerificationMethod::ModelClaim, "{none:?}");

    // A question about code is an information task: the tests are not the measure.
    let mut calls = read_and_edit();
    calls.push(cargo_test(CARGO_FAIL_OUT));
    let question = evaluate_run("why does the cargo test fail in the order parser", &coding_run("It fails because the parser drops the last token.", calls));
    assert_ne!(question.method, VerificationMethod::Tests, "{question:?}");
}

#[test]
fn a_verified_coding_run_is_learned_with_safe_steps_and_a_coding_task_type() {
    let s = svc("coding_learn");
    let mut calls = read_and_edit();
    calls.push(("term.run", json!({ "exe": "cargo", "args": ["test", "--lib", "--token", "sk-abcdefghijklmnop123456"] }), CARGO_PASS));
    let out = run_with_service(&s, CODING_GOAL, |exp| {
        exp.prepare_block(CODING_GOAL, &coding_tools());
        Ok(coding_run("Fixed the parser; all tests pass.", calls.clone()))
    })
    .unwrap();
    assert_eq!(out.stopped_reason, "final");
    drain(&s);
    let all = experiences(&s);
    assert_eq!(all.len(), 1);
    let e = &all[0];
    assert_eq!((e.task_type.as_str(), e.status, e.verification_method), ("coding", ExperienceStatus::Verified, VerificationMethod::Tests));
    assert!(e.important_steps.iter().any(|l| l == "term.run cargo (tests)"), "{:?}", e.important_steps);
    assert!(e.constraints.iter().any(|c| c.contains("run them again after every change")), "{:?}", e.constraints);
    assert!(e.tools_used.contains(&"term.run".to_string()) && e.tools_used.contains(&"fs.edit".to_string()), "{:?}", e.tools_used);
    let stored = serde_json::to_string(e).unwrap();
    assert!(!stored.contains("sk-abcdef") && !stored.contains("--token") && !stored.contains("src/parser.rs"), "arguments and paths stay out of memory: {stored}");
    // Verification by tests outranks page evidence in quality.
    assert!(e.quality_score > 0.8, "{}", e.quality_score);
}

#[test]
fn a_failed_coding_run_is_not_learned_and_a_similar_coding_task_retrieves_only_coding_experience() {
    let s = svc("coding_reuse");
    // A failed attempt leaves nothing behind.
    let mut failing = read_and_edit();
    failing.push(cargo_test(CARGO_FAIL_OUT));
    run_with_service(&s, CODING_GOAL, |exp| {
        exp.prepare_block(CODING_GOAL, &coding_tools());
        Ok(coding_run("Done.", failing.clone()))
    })
    .unwrap();
    drain(&s);
    assert!(experiences(&s).is_empty());
    // A verified one is learned; a web task is learned beside it.
    let mut passing = read_and_edit();
    passing.push(cargo_test(CARGO_PASS));
    run_with_service(&s, CODING_GOAL, |exp| {
        exp.prepare_block(CODING_GOAL, &coding_tools());
        Ok(coding_run("Fixed.", passing.clone()))
    })
    .unwrap();
    run_with_service(&s, GOAL, |exp| {
        exp.prepare_block(GOAL, &tools());
        Ok(good())
    })
    .unwrap();
    drain(&s);
    assert_eq!(experiences(&s).len(), 2);
    // A similar coding task gets the coding experience, as a note to weigh; the market one is not offered.
    let similar = "fix the failing test in the invoice parser";
    let mut block = String::new();
    let mut offered = 0;
    run_with_service(&s, similar, |exp| {
        block = exp.prepare_block(similar, &coding_tools());
        offered = exp.used.len();
        Ok(coding_run("Fixed.", passing.clone()))
    })
    .unwrap();
    assert_eq!(offered, 1, "{block}");
    assert!(block.contains("term.run cargo (tests)") && block.contains("not an instruction"), "{block}");
    assert!(!block.to_lowercase().contains("google"), "{block}");
    // And a web task is not offered the coding experience.
    let mut web_block = String::new();
    run_with_service(&s, "what is the stock market condition today", |exp| {
        web_block = exp.prepare_block("what is the stock market condition today", &tools());
        Ok(good())
    })
    .unwrap();
    assert!(!web_block.contains("term.run"), "{web_block}");
    drain(&s);
    // The reuse was credited to the coding experience, verified by its own tests.
    let coding = experiences(&s).into_iter().find(|e| e.task_type == "coding").unwrap();
    assert_eq!((coding.reuse_count, coding.successful_reuse_count, coding.failed_reuse_count), (1, 1, 0));
}

// ---- a run that failed must never be learned as a success -------------------------------------------

/// What really happened live: the agent could not find the file, searched, gave up, and was still
/// recorded as a VERIFIED experience because "a host tool returned something".
fn gave_up() -> McpAgentResult {
    coding_run(
        "The file was not found, so the test could not be run.",
        vec![
            ("fs.read", json!({ "path": "src/lib.rs" }), "ERROR [NOT_FOUND] fs.read: C:/ws/src/lib.rs: The system cannot find the path specified."),
            ("fs.find", json!({ "path": "scratch/project", "name": "lib.rs" }), "No matches."),
        ],
    )
}

#[test]
fn a_coding_run_that_gave_up_or_never_ran_a_check_is_unknown_never_verified() {
    let v = evaluate_run(CODING_GOAL, &gave_up());
    assert!(!v.verified && v.method == VerificationMethod::ModelClaim && v.result.contains("could not be done"), "{v:?}");
    // Unknown, not a failure: it must not count against an experience that was offered.
    assert_eq!(v.method, VerificationMethod::ModelClaim);
    // It read and edited, said it was fixed, and never ran the tests: still only its word.
    let no_check = evaluate_run(CODING_GOAL, &coding_run("Fixed the add function.", read_and_edit()));
    assert!(!no_check.verified && no_check.result.contains("no test, build, type check or lint run"), "{no_check:?}");
    // Read-only tools returning text is not evidence either.
    let only_reads = evaluate_run(CODING_GOAL, &coding_run("The function was corrected.", vec![("fs.read", json!({ "path": "a.rs" }), "fn add() {}")]));
    assert!(!only_reads.verified, "{only_reads:?}");
    // A real passing check still wins.
    let mut ok = read_and_edit();
    ok.push(cargo_test(CARGO_PASS));
    assert!(evaluate_run(CODING_GOAL, &coding_run("Fixed.", ok)).verified);
    // The give-up rule applies to information tasks as well.
    let info = evaluate_run(GOAL, &run_result("final", "Sorry, the market figures could not be found on the page."));
    assert!(!info.verified && info.method == VerificationMethod::ModelClaim, "{info:?}");
    for admitting in ["I couldn't open the page.", "Unable to read the file.", "The site was not found.", "It failed to compile."] {
        assert!(answer_admits_failure(admitting), "{admitting}");
    }
    for fine in ["The Nifty 50 is at 22,716.20.", "Fixed the add function; all 14 tests pass.", "The weather in Pune is 30 degrees."] {
        assert!(!answer_admits_failure(fine), "{fine}");
    }
}

#[test]
fn a_gave_up_coding_run_creates_no_experience_and_does_not_debit_the_one_it_used() {
    let s = svc("gave_up");
    // One good experience first.
    let mut passing = read_and_edit();
    passing.push(cargo_test(CARGO_PASS));
    run_with_service(&s, CODING_GOAL, |exp| {
        exp.prepare_block(CODING_GOAL, &coding_tools());
        Ok(coding_run("Fixed.", passing.clone()))
    })
    .unwrap();
    drain(&s);
    assert_eq!(experiences(&s).len(), 1);
    let before = experiences(&s)[0].clone();
    // A similar task uses it, cannot find its files and gives up.
    run_with_service(&s, "fix the failing test in the invoice parser", |exp| {
        exp.prepare_block("fix the failing test in the invoice parser", &coding_tools());
        Ok(gave_up())
    })
    .unwrap();
    drain(&s);
    let all = experiences(&s);
    assert_eq!(all.len(), 1, "the give-up was not learned as a success: {:?}", all.iter().map(|e| (&e.problem_summary, &e.important_steps)).collect::<Vec<_>>());
    assert_eq!((all[0].failed_reuse_count, all[0].confidence), (0, before.confidence), "and an unknown outcome does not lower trust in what it used");
}

/// Found in a review: a Stop pressed while the model was planning ended the run as "llm_error: stopped: the task was cancelled", which counted as a
/// failure (35 % off the confidence of the experience that was offered, and three of them retired it), and the same went for a model that was busy
/// or failed. None of those says anything about the approach.
#[test]
fn a_run_that_the_model_or_the_person_cut_short_neither_teaches_nor_debits() {
    use crate::modules::agent_experience::ended_without_an_outcome;
    for reason in ["cancelled", "client_disconnected", "deadline", "limit", "llm_error: the language model is busy with another request (waited 120s)", "llm_error: stopped: the task was cancelled", "parse_error_on_last_step: no json"] {
        assert!(ended_without_an_outcome(reason), "{reason}");
    }
    for reason in ["final", "max_steps", "experiment_limit", "learned_fast_path", "laya_gate", "scripted_open_app", ""] {
        assert!(!ended_without_an_outcome(reason), "{reason}");
    }
}

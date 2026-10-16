//! Tests of `request_json.rs`: what a request says about thinking, the cache and the slot, and the question for the probabilities (move only: they were the inline module `thinking_tests` and part of the inline module `tests` of `llama_server.rs`; the path of every test is now `modules::llm_router::llama_server::request_json_tests::<name>`).

use super::*;

#[test]
fn a_request_says_thinking_is_off_unless_the_server_was_started_to_think_and_nothing_else_changes() {
    let off = request_body_with("p", 100, Some(1), None, false);
    assert_eq!(off["chat_template_kwargs"], json!({ "enable_thinking": false }), "the body that was always sent");
    let on = request_body_with("p", 100, Some(1), None, true);
    assert_eq!(on["chat_template_kwargs"], json!({ "enable_thinking": true }));
    // Only that one field differs, with a slot and with a constraint too.
    let mut a = off.clone();
    let mut b = on.clone();
    a["chat_template_kwargs"] = json!(null);
    b["chat_template_kwargs"] = json!(null);
    assert_eq!(a, b);
    let grammar = Constraint::Grammar("root ::= \"a\"".to_string());
    let (mut c, mut d) = (request_body_with("p", 100, None, Some(&grammar), false), request_body_with("p", 100, None, Some(&grammar), true));
    assert_eq!(c["grammar"], d["grammar"]);
    c["chat_template_kwargs"] = json!(null);
    d["chat_template_kwargs"] = json!(null);
    assert_eq!(c, d);
}

#[test]
fn a_decision_request_asks_for_the_numbers_and_a_steady_decode_and_a_grammar_of_the_labels() {
    let plain = request_body_constrained("p", 1, None, None);
    assert_eq!(with_logprobs(plain.clone(), None), plain, "without it the request is as it was");
    let body = with_logprobs(request_body_constrained("p", 1, None, Some(&Constraint::Grammar(choice_grammar(&["A".into(), "B".into()])))), Some(50));
    assert_eq!((body["logprobs"].clone(), body["top_logprobs"].clone(), body["temperature"].clone(), body["top_p"].clone()), (json!(true), json!(20), json!(0), json!(1)), "never more than the server gives");
    assert_eq!(body["grammar"], "root ::= \"A\" | \"B\"");
    assert_eq!(choice_grammar(&["x\"y".into()]), "root ::= \"x\\\"y\"", "a quote in a label cannot end the string");
}

#[test]
fn the_request_asks_for_the_cache_and_no_thinking() {
    let b = request_body("hello", 120);
    assert_eq!(b["messages"][0]["role"], "user");
    assert_eq!(b["messages"][0]["content"], "hello");
    assert_eq!(b["max_tokens"], 120);
    assert_eq!(b["cache_prompt"], true);
    assert_eq!(b["stream"], false);
    assert_eq!(b["chat_template_kwargs"]["enable_thinking"], false);
    assert!(b["temperature"].as_f64().unwrap() <= 0.5);
}

#[test]
fn a_request_can_name_the_cache_slot_it_belongs_to() {
    assert!(request_body("hi", 8).get("id_slot").is_none(), "no preference unless asked");
    assert_eq!(request_body_for_slot("hi", 8, Some(AGENT_SLOT))["id_slot"], 1);
    assert_eq!(request_body_for_slot("hi", 8, Some(CHAT_SLOT))["id_slot"], 0);
    assert_ne!(CHAT_SLOT, AGENT_SLOT);
    // The body is otherwise the same.
    assert_eq!(request_body_for_slot("hi", 8, Some(1))["max_tokens"], 8);
    // What the server measured, read from its reply.
    let reply = json!({"timings": {"prompt_n": 12, "cache_n": 800, "predicted_n": 40, "prompt_ms": 310.4, "predicted_ms": 10_000.0}});
    let t = timings_of(&reply);
    assert_eq!((t.prompt_n, t.cache_n, t.predicted_n, t.prompt_ms, t.predicted_ms), (12, 800, 40, 310, 10_000));
    assert!((t.decode_tokens_per_second() - 4.0).abs() < 1e-9);
    assert_eq!(timings_of(&json!({})), Timings::default(), "a reply with no timings measured nothing");
    assert_eq!(Timings::default().decode_tokens_per_second(), 0.0);
}

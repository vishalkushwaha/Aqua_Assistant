//! Tests of `request_outcome.rs`: the reply and what it carries, and the sentences of a request that was refused, was busy, gave the model up or went quiet (move only: they were the inline module `timings_note_tests` and parts of the inline modules `tests` and `stream_tests` of `llama_server.rs`; the path of every test is now `modules::llm_router::llama_server::request_outcome_tests::<name>`).

use super::*;

#[test]
fn the_probabilities_of_the_first_word_are_read_from_either_shape_of_reply_most_likely_first() {
    let chat = json!({"choices": [{"message": {"content": "B"}, "logprobs": {"content": [{"token": "B", "logprob": -0.1, "top_logprobs": [
        {"token": "A", "logprob": -3.0}, {"token": "B", "logprob": -0.1}, {"token": " C", "logprob": -5.0}]}]}}]});
    let got = top_probs_of(&chat);
    assert_eq!(got.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(), ["B", "A", " C"]);
    assert!((got[0].1 - (-0.1f64).exp()).abs() < 1e-9 && got[0].1 > 0.9, "{got:?}");
    let native = json!({"completion_probabilities": [{"content": "A", "probs": [{"tok_str": "A", "prob": 0.7}, {"tok_str": "B", "prob": 0.2}]}]});
    assert_eq!(top_probs_of(&native), vec![("A".to_string(), 0.7), ("B".to_string(), 0.2)]);
    let native_logprobs = json!({"completion_probabilities": [{"top_logprobs": [{"token": "A", "logprob": 0.0}]}]});
    assert_eq!(top_probs_of(&native_logprobs), vec![("A".to_string(), 1.0)]);
    assert!(top_probs_of(&json!({"choices": [{"message": {"content": "A"}}]})).is_empty(), "a reply without numbers has none");
    assert!(top_probs_of(&json!({"choices": [{"logprobs": {"content": [{"top_logprobs": [{"token": "A", "logprob": "no"}, {"token": "B"}]}]}}]})).is_empty(), "what is not a number is left out");
}

#[test]
fn a_reply_that_ran_out_of_tokens_is_recognised() {
    assert!(reply_was_cut_off(&json!({"choices": [{"finish_reason": "length", "message": {"content": "{\"tool\":"}}]})));
    assert!(!reply_was_cut_off(&json!({"choices": [{"finish_reason": "stop", "message": {"content": "ok"}}]})));
    assert!(!reply_was_cut_off(&json!({"choices": [{"message": {"content": "ok"}}]})), "no reason given: not cut off");
    assert!(!reply_was_cut_off(&json!({"error": "nope"})));
}

#[test]
fn replies_are_parsed_with_their_cache_counts() {
    let v = json!({"choices": [{"message": {"role": "assistant", "content": "{\"tool\":\"x\"}"}}], "timings": {"prompt_n": 16, "cache_n": 1011}});
    assert_eq!(parse_reply(&v), Some(("{\"tool\":\"x\"}".to_string(), 16, 1011)));
    assert_eq!(parse_reply(&json!({"choices": [{"message": {"content": "ok"}}]})), Some(("ok".to_string(), 0, 0)));
    assert!(parse_reply(&json!({"error": "nope"})).is_none());
    assert!(parse_reply(&json!({"choices": []})).is_none());
}

#[test]
fn the_servers_own_speed_numbers_are_put_in_the_log_line() {
    let reply = json!({"timings": {"prompt_n": 672, "prompt_per_second": 36.04, "predicted_n": 87, "predicted_per_second": 4.634}});
    assert_eq!(timings_note(&reply), " - generated 87 tokens at 4.6/s, read at 36/s");
    let cached = json!({"timings": {"prompt_n": 14, "prompt_per_second": 20.0, "predicted_n": 40, "predicted_per_second": 4.7}});
    assert_eq!(timings_note(&cached), " - generated 40 tokens at 4.7/s", "14 tokens read is too few to give a speed");
}

#[test]
fn a_reply_without_timings_adds_nothing() {
    assert_eq!(timings_note(&json!({"choices": []})), "");
    assert_eq!(timings_note(&json!({"timings": {"prompt_n": 5}})), "");
}

#[test]
fn only_a_4xx_answer_counts_as_the_server_refusing_one_request() {
    assert!(is_client_rejection("HTTP 400: prompt is too long"));
    assert!(is_client_rejection("HTTP 429: busy"));
    assert!(!is_client_rejection("HTTP 500: crashed"));
    assert!(!is_client_rejection("HTTP 5xx"));
    assert!(!is_client_rejection("connection refused"));
    assert!(!is_client_rejection("HTTP 4"));
}

/// Card OA4-02 (CX-03): a prompt that did not fit says how many tokens it had and how many the window holds, so the planner can recover and the meter can learn; any other
/// refusal keeps what the server said, as it always did.
#[test]
fn a_refusal_of_a_prompt_that_is_too_long_names_its_tokens_and_the_window_and_any_other_keeps_the_servers_words() {
    let body = r#"HTTP 400: {"error":{"code":400,"message":"the request exceeds the available context size, try increasing it","type":"exceed_context_size_error","n_prompt_tokens":6234,"n_ctx":6144}}"#;
    assert_eq!(overflow_counts(body), Some((6234, 6144)));
    // With spaces after the colons, as another JSON writer would put them.
    assert_eq!(overflow_counts(r#"HTTP 400: {"error": {"type": "exceed_context_size_error", "n_prompt_tokens": 9000, "n_ctx": 6144}}"#), Some((9000, 6144)));
    let said = refusal_message(body);
    assert_eq!(said, "the language model refused this request (HTTP 400: exceeds the available context size (6144 tokens; the prompt has 6234))");
    // The model layer reads it as an overflow with the window as its limit, and the meter reads the count.
    assert_eq!(crate::model::error::context_overflow(&said), Some(Some(6144)));
    assert_eq!(crate::modules::context::meter::refusal_prompt_tokens(&said), Some(6234));
    // A refusal that is not about the size, or that has no numbers, is the first 160 characters of what the server said.
    let other = r#"HTTP 400: {"error":{"code":400,"message":"unsupported response_format"}}"#;
    assert_eq!(overflow_counts(other), None);
    assert_eq!(refusal_message(other), format!("the language model refused this request ({other})"));
    assert_eq!(overflow_counts("HTTP 400: the request exceeds the available context size (2048 tokens)"), None, "no numbers to read");
    assert!(refusal_message("HTTP 400: the request exceeds the available context size (2048 tokens)").contains("exceeds the available context size (2048 tokens)"));
    let long = format!("HTTP 400: {}", "x".repeat(400));
    assert_eq!(refusal_message(&long).chars().count(), "the language model refused this request ()".chars().count() + 160);
}

#[test]
fn a_request_that_waited_for_the_model_and_was_not_served_says_so_in_words_that_are_found_again() {
    assert_eq!(busy_message(120), "the language model is busy with another request (waited 120s)");
    assert!(is_busy_text(&busy_message(5)) && is_busy_text(&format!("llm_error: {}", busy_message(5))));
    assert!(!is_busy_text("the language model is busy with another request, so a decision is not waited for"));
    assert!(!is_busy_text("llama timed out after 90s"));
}

/// Card OA4-02 (CX-03): the guard against a runaway paste follows the window and is never above the old fixed limit.
#[test]
fn the_limit_on_a_prompts_length_follows_the_window_and_is_at_most_the_fixed_one() {
    assert_eq!((ceiling_for(2048), ceiling_for(4096), ceiling_for(6144), ceiling_for(8192), ceiling_for(32768)), (12_288, 24_576, 30_000, 30_000, 30_000));
    assert_eq!(ceiling_for(6144), MAX_PROMPT_CHARS, "at the usual window it is the limit there has always been");
    assert!(crate::model::error::context_overflow(&too_long(40_000)).is_some(), "Aqua's own guard still reads as a prompt that does not fit");
}

#[test]
fn the_message_of_a_quiet_stream_is_found_again_and_names_the_setting() {
    let mid = idle_message(false, 20, true);
    let first = idle_message(true, 90, false);
    assert!(is_idle_text(&mid) && is_idle_text(&first) && is_idle_text(&format!("llm_error: {mid}")));
    assert!(mid.contains("in the middle of the answer") && mid.contains("nothing came for 20s") && mid.contains("AQUA_STREAM_IDLE_SECS") && mid.contains("is still loaded"), "{mid}");
    assert!(first.contains("before it began to answer") && first.contains("nothing came for 90s") && first.contains("AQUA_STREAM_FIRST_EVENT_SECS") && first.contains("was stopped"), "{first}");
    for other in ["llama timed out after 300s".to_string(), busy_message(5), yielded_message(), "the model server stopped in the middle of the answer (3 characters had come)".to_string()] {
        assert!(!is_idle_text(&other), "{other}");
    }
}

use super::generation_speed;
use crate::modules::llm_router::llama_server::Timings;

#[test]
fn the_speed_is_the_servers_own_figure_when_it_measured_one_and_a_guess_that_says_so_when_it_did_not() {
    // 40 tokens in 8 seconds is 5 a second, whatever the text looks like and however long the request waited before it started.
    let t = Timings { prompt_n: 900, cache_n: 0, predicted_n: 40, prompt_ms: 20_000, predicted_ms: 8_000 };
    assert_eq!(generation_speed(Some(t), "a short answer", 30_000), (5.0, false));
    let third = Timings { predicted_n: 10, predicted_ms: 3_000, ..t };
    assert_eq!(generation_speed(Some(third), "x", 1_000), (3.3, false), "to a tenth");
    // No numbers (the answer came from llama-cli) or numbers of nothing: the old guess, (chars / 4) / seconds, marked as one.
    let text = "x".repeat(400);
    assert_eq!(generation_speed(None, &text, 10_000), (10.0, true));
    assert_eq!(generation_speed(Some(Timings::default()), &text, 10_000), (10.0, true));
    assert_eq!(generation_speed(Some(Timings { predicted_n: 5, predicted_ms: 0, ..t }), &text, 10_000), (10.0, true), "no time measured is not a speed");
}

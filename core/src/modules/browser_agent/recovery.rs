//! What the loop does when the request for the next step fails: ask again with less of the results shown, wait for a model that is busy, give the model up to a person who is waiting for it, or end the run (card OA3-05, move only).

#[cfg(test)]
use super::*;

/// The most times one run gives the model up to a person who is waiting for it (card R6-03, T6-03.4); after that its calls are not asked to yield again.
pub(super) const MAX_YIELDS_PER_RUN: u32 = 6;

/// Is this the error that ends a model call because the person pressed Stop (or the caller hung up)?
pub(super) fn is_stop_error(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("the task was cancelled")
}

/// What the loop does about a request for the next step that failed (card OA4-02, tasks CX-03 and AG-10 of the OpenCode audit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Recovery {
    /// The server refused a prompt that did not fit the window: show less of the results and ask again. Once in a round.
    Shrink,
    /// The model was busy or could not be used: wait this many milliseconds and ask again.
    Wait(u64),
    /// Nothing more to try: the run ends as it did before (with the steps it has made).
    GiveUp,
}

/// The most steps down in how much of the results is shown that refusals may ask for in one run; past it the shrinking has reached its floors.
pub(super) const MAX_EXTRA_SHRINKS: u32 = 16;

/// What to do about an error of the given class. `recovered_this_round`: a refusal of a prompt that did not fit was already recovered from in this round (a second one ends the run).
/// `transient_failures`: requests in a row that failed for a reason a wait may cure; three waits (2, 4 and 8 seconds, `coding_driver::retry_delay_ms`) and then the run ends. The
/// person's Stop, a request the server refused for another reason, and anything else are not asked again.
pub(super) fn recovery_for(kind: crate::modules::planner::PlanErrorKind, recovered_this_round: bool, transient_failures: u32, extra_shrinks: u32) -> Recovery {
    use crate::modules::planner::PlanErrorKind;
    match kind {
        PlanErrorKind::ContextOverflow if !recovered_this_round && extra_shrinks < MAX_EXTRA_SHRINKS => Recovery::Shrink,
        k if k.worth_waiting_for() && transient_failures < crate::modules::coding_driver::PLANNER_RETRIES => crate::modules::coding_driver::retry_delay_ms(transient_failures).map_or(Recovery::GiveUp, Recovery::Wait),
        _ => Recovery::GiveUp,
    }
}

/// The token meter learns from a refusal what the prompt cost: the count the server named ("the prompt has N tokens"), else that the prompt was more than the window it names. A
/// refusal that names neither (Aqua's own guard against a runaway paste) teaches nothing.
pub(super) fn learn_from_refusal(prompt: &str, said: &str) {
    use crate::modules::context::meter::{meter, refusal_prompt_tokens};
    let m = meter();
    if let Some(tokens) = refusal_prompt_tokens(said) {
        m.observe(prompt, tokens);
        m.observe_at_least(prompt, tokens);
    } else if let Some(Some(window)) = crate::model::error::context_overflow(said) {
        m.observe_at_least(prompt, u64::from(window) + 1);
    }
}

/// `aqua_context_overflow_total{recovered}`: a prompt the server refused as too long, and whether asking again with less of the results got an answer. (No text of the prompt is in it.)
pub(super) fn count_context_overflow(recovered: bool) {
    crate::metrics::inc("aqua_context_overflow_total", &[("recovered", if recovered { "true" } else { "false" })]);
}

/// Waits `ms` milliseconds before the model is asked again, in steps of 250 so that a Stop (`stop`) ends the wait within a quarter of a second. `false` when it was stopped.
pub(super) fn wait_unless_stopped(ms: u64, stop: &dyn Fn() -> bool) -> bool {
    #[cfg(test)]
    let ms = ms * backoff_percent() / 100;
    let mut left = ms;
    while left > 0 {
        if stop() {
            return false;
        }
        let step = left.min(250);
        std::thread::sleep(std::time::Duration::from_millis(step));
        left -= step;
    }
    !stop()
}

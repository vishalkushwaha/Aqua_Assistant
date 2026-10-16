//! The escalation ladder: retry, another way, a diagnosis, ask the person, stop safely. Each rung has a
//! limit, so nothing loops forever, and some failures skip rungs that cannot help.

use super::classify::ErrorClass;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// Try the same step again after this many seconds.
    Retry { after_secs: u64 },
    /// Try a different, safe way (restart the browser, refresh the session, look for the control again).
    Alternative { what: &'static str },
    /// Have the assistant read the error and propose a fix for approval.
    Diagnose,
    /// Hand off to the browser_agent to read documentation or search the web for the error. Not built: the flow engine does not start a research for this rung, it says so in the
    /// log and fails the step with its own error (and no class that the engine retries reaches the rung).
    ResearchDocs,
    /// A person has to decide or supply something.
    AskUser,
    /// Nothing more is safe to try: stop, keep everything done so far, say why.
    StopSafely,
}

/// Where a failing step stands.
#[derive(Debug, Clone, Copy)]
pub struct Situation {
    pub class: ErrorClass,
    /// Attempts already made on this step in this run (the first failure is 1).
    pub attempts: u32,
    /// The step reaches outside Aqua and it is not known whether it happened. Never retried.
    pub outcome_unknown: bool,
    /// A recovery that can be tried automatically exists for this class (S3 plugs these in).
    pub has_alternative: bool,
    /// The alternative has been tried already.
    pub alternative_tried: bool,
    /// A diagnosis has been made already.
    pub diagnosed: bool,
    /// A person has been asked already and it did not settle it.
    pub asked: bool,
    /// Whether the web/documentation was already researched.
    pub researched: bool,
}

impl Situation {
    pub fn first(class: ErrorClass, attempts: u32) -> Self {
        Self { class, attempts, outcome_unknown: false, has_alternative: false, alternative_tried: false, diagnosed: false, asked: false, researched: false }
    }
}

/// How many plain retries a class gets, and how long to wait before each.
fn retries(class: ErrorClass) -> &'static [u64] {
    match class {
        ErrorClass::Network | ErrorClass::Timeout | ErrorClass::ExternalService => &[5, 30, 120],
        ErrorClass::RateLimit => &[60, 300],
        ErrorClass::BrowserError => &[5, 30],
        ErrorClass::ApiError => &[10],
        ErrorClass::Unknown => &[10],
        // A wrong login, a missing file or a bad setting will not fix itself.
        ErrorClass::Authentication | ErrorClass::PermissionError | ErrorClass::InvalidInput | ErrorClass::MissingFile | ErrorClass::WebsiteUiChange => &[],
    }
}

fn alternative(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::Authentication => "sign in again",
        ErrorClass::BrowserError => "restart the browser",
        ErrorClass::WebsiteUiChange => "look for the control by its meaning instead of its old address",
        ErrorClass::Network | ErrorClass::Timeout | ErrorClass::ExternalService => "try again through a fresh connection",
        _ => "use another way",
    }
}

/// The next rung. Called after each failure; the answer for attempt N+1 follows from the situation.
pub fn decide(s: &Situation) -> Action {
    if s.outcome_unknown {
        // It may already have happened (a post, a message). Only a person can check.
        return if s.asked { Action::StopSafely } else { Action::AskUser };
    }
    let waits = retries(s.class);
    if (s.attempts as usize) <= waits.len() && !waits.is_empty() {
        return Action::Retry { after_secs: waits[s.attempts as usize - 1] };
    }
    if s.has_alternative && !s.alternative_tried {
        return Action::Alternative { what: alternative(s.class) };
    }
    // Diagnosing is for failures a reading might explain; a lapsed login or a denial is the person's to fix.
    let diagnosable = !matches!(s.class, ErrorClass::Authentication | ErrorClass::PermissionError | ErrorClass::RateLimit);
    if diagnosable && !s.diagnosed {
        return Action::Diagnose;
    }
    // TASK-501: Seamless handoff to browser_agent for documentation search.
    let needs_research = matches!(s.class, ErrorClass::ApiError | ErrorClass::InvalidInput | ErrorClass::WebsiteUiChange);
    if needs_research && !s.researched {
        return Action::ResearchDocs;
    }
    if !s.asked {
        return Action::AskUser;
    }
    Action::StopSafely
}

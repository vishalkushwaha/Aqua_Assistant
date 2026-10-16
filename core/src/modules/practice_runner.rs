//! Runs the agent's own practice tasks in the background.
//!
//! The decision of *what* to practise and *when* is in `memory::experience::practice`; this module
//! does the running. A practice run:
//!   - is unattended: every approval is refused on the spot (nothing is shown, nothing is waited for,
//!     nothing is ever granted);
//!   - gets the browser only: no files, terminal, git, databases or desktop;
//!   - makes one attempt, within a planning-round limit and the time limit;
//!   - stops as soon as a user's task starts, the practice or learning switch goes off, or time is up.

use super::agent_experience::{evaluate_run, last_user_activity, practice_scope, user_run_active};
use super::browser_agent::{run_mcp_agent_shared_with_progress, AgentStep, McpAgentResult};
use super::llm_router::LlamaRouterEngine;
use crate::memory::experience::model::now_secs;
use crate::memory::experience::practice::{self, GeneratedTask, Gate, PracticeOutcome, TickResult};
use crate::memory::experience::sanitize::redact;
use crate::memory::experience::service::service;
use crate::memory::experience::settings::LearningSettings;
use aqua_mcp_adapters::browser::BrowserBackend;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

static FORCE: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
static LAST_NOTE: Mutex<String> = Mutex::new(String::new());

/// How often the background loop looks at whether to practise.
const CHECK_EVERY: Duration = Duration::from_secs(15);

/// "Run one now" from the UI: the next check skips the idle wait and the gap (nothing else).
pub fn request_run_now() {
    FORCE.store(true, Ordering::SeqCst);
    set_note("run requested; starting at the next check");
}

pub fn last_note() -> String {
    LAST_NOTE.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

fn set_note(text: &str) {
    *LAST_NOTE.lock().unwrap_or_else(PoisonError::into_inner) = text.to_string();
}

/// The gate for a status check that is not about to run anything.
pub fn current_gate() -> Gate {
    Gate { now: now_secs(), last_user_activity: last_user_activity(), user_active: user_run_active(), force: false }
}

/// Start the background loop once. It sleeps almost all of the time and does nothing unless auto
/// practice is switched on.
pub fn spawn(llm: Arc<LlamaRouterEngine>, browser: Arc<BrowserBackend>) {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    practice::recover(&service());
    let spawned = std::thread::Builder::new().name("aqua-practice".into()).spawn(move || loop {
        std::thread::sleep(CHECK_EVERY);
        let svc = service();
        let force = FORCE.swap(false, Ordering::SeqCst);
        if !force && !svc.settings().may_practice() {
            continue;
        }
        let gate = Gate { now: now_secs(), last_user_activity: last_user_activity(), user_active: user_run_active(), force };
        let result = practice::tick(&svc, &gate, |task, settings| run_agent(&llm, &browser, task, settings));
        match result {
            TickResult::Skipped(skip) => {
                if force {
                    set_note(&format!("not run: {}", skip.describe()));
                }
            }
            TickResult::Ran { task, outcome, .. } => {
                set_note(&format!("practised \"{}\": {}", task.goal, outcome.status));
            }
        }
    });
    if spawned.is_err() {
        STARTED.store(false, Ordering::SeqCst);
        tracing::warn!("could not start the practice thread");
    }
}

/// Why a running practice task should stop now, if it should.
fn stop_reason(started: Instant, limit: Duration) -> Option<&'static str> {
    if user_run_active() {
        Some("a user task started")
    } else if !service().settings().may_practice() {
        Some("auto practice was switched off")
    } else if started.elapsed() >= limit {
        Some("the time limit was reached")
    } else {
        None
    }
}

fn run_agent(llm: &Arc<LlamaRouterEngine>, browser: &Arc<BrowserBackend>, task: &GeneratedTask, settings: &LearningSettings) -> PracticeOutcome {
    let goal = task.goal.clone();
    let (llm2, browser2, goal2) = (Arc::clone(llm), Arc::clone(browser), goal.clone());
    let max_steps = settings.practice_max_steps;
    let limit = Duration::from_secs(settings.max_execution_secs as u64);
    let (tx, rx) = channel::<AgentStep>();
    let handle = match std::thread::Builder::new().name("aqua-practice-run".into()).spawn(move || {
        // The flags are per thread, so they are set on the thread that runs the agent.
        let _scope = practice_scope();
        run_mcp_agent_shared_with_progress(&llm2, Some(browser2), None, &goal2, max_steps, None, Some(tx))
    }) {
        Ok(h) => h,
        Err(e) => return PracticeOutcome::failed(format!("could not start the run: {e}")),
    };

    // Watch it. Dropping the receiver tells the agent its listener is gone; it stops at its next step.
    let started = Instant::now();
    let mut rx = Some(rx);
    let mut stopped_because: Option<&'static str> = None;
    while let Some(r) = rx.as_ref() {
        match r.recv_timeout(Duration::from_secs(1)) {
            Ok(_) => {}
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                if let Some(why) = stop_reason(started, limit) {
                    stopped_because = Some(why);
                    rx = None;
                }
            }
        }
    }
    drop(rx);
    let joined = handle.join();
    outcome_of(&goal, joined, stopped_because)
}

fn outcome_of(goal: &str, joined: std::thread::Result<anyhow::Result<McpAgentResult>>, stopped_because: Option<&str>) -> PracticeOutcome {
    let result = match joined {
        Err(_) => return PracticeOutcome::failed("the practice run panicked (contained)"),
        Ok(Err(e)) => {
            let (clean, _) = redact(&e.to_string());
            return PracticeOutcome::failed(clean.chars().take(200).collect::<String>());
        }
        Ok(Ok(r)) => r,
    };
    let steps = result.steps.iter().filter(|s| s.kind == "tool").count() as u32;
    if stopped_because.is_some() || matches!(result.stopped_reason.as_str(), "cancelled" | "client_disconnected") {
        return PracticeOutcome { status: "interrupted".into(), verified: false, steps, note: format!("stopped: {}", stopped_because.unwrap_or("cancelled")) };
    }
    // A practice run has no one to ask, so anything that needs an approval ends the run as refused.
    // (Sites you have chosen to "always allow" never need one; anything else does.)
    let declined = result.stopped_reason == "user_declined" || result.final_answer.contains("you declined");
    if declined {
        let what = result.final_answer.split("declined: ").nth(1).unwrap_or("an action").trim_end_matches('.').to_string();
        return PracticeOutcome {
            status: "refused".into(),
            verified: false,
            steps,
            note: format!("needed an approval it cannot give ({what}). Run a web task yourself once and choose Always allow for the site."),
        };
    }
    let verification = evaluate_run(goal, &result);
    if verification.verified {
        return PracticeOutcome { status: "verified".into(), verified: true, steps, note: verification.result };
    }
    // Refused because it would have needed an approval nobody was there to give?
    let refused = result.steps.iter().filter_map(|s| s.detail.get("observation").and_then(|o| o.as_str())).any(|o| {
        let l = o.to_ascii_lowercase();
        l.contains("denied") || l.contains("not approved") || l.contains("approval")
    });
    let status = if refused { "refused" } else { "unverified" };
    PracticeOutcome { status: status.into(), verified: false, steps, note: verification.result }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(stopped: &str, answer: &str, obs: &str) -> McpAgentResult {
        McpAgentResult {
            final_answer: answer.into(),
            steps: vec![AgentStep { step: 1, kind: "tool".into(), detail: serde_json::json!({ "tool": "browser_navigate", "arguments": {}, "observation": obs }) }],
            tool_rounds: 1,
            stopped_reason: stopped.into(),
            gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
        }
    }

    #[test]
    fn a_stopped_run_is_interrupted_not_failed_and_a_run_error_is_contained() {
        let r = result("client_disconnected", "Stopped.", "Page URL: https://www.google.com/search?q=x");
        let o = outcome_of("bitcoin price today", Ok(Ok(r)), None);
        assert_eq!((o.status.as_str(), o.verified), ("interrupted", false));
        let o = outcome_of("bitcoin price today", Ok(Ok(result("final", "x", "y"))), Some("a user task started"));
        assert_eq!(o.status, "interrupted");
        assert!(o.note.contains("a user task started"));
        let o = outcome_of("bitcoin price today", Ok(Err(anyhow::anyhow!("browser said password=hunter2"))), None);
        assert!(o.status == "failed" && !o.note.contains("hunter2"), "{o:?}");
    }

    #[test]
    fn a_verified_answer_is_verified_and_a_refused_approval_is_labelled() {
        let page = "Page URL: https://www.google.com/search?q=bitcoin%20price%20today\nBitcoin price today BTC/USD 84,293.33 (-0.10%)";
        let good = result("final", "Bitcoin is at 84,293.33, down 0.10%.", page);
        let o = outcome_of("bitcoin price today", Ok(Ok(good)), None);
        assert_eq!((o.status.as_str(), o.verified), ("verified", true), "{o:?}");
        let invented = result("final", "Bitcoin is at 99,999.99.", page);
        assert_eq!(outcome_of("bitcoin price today", Ok(Ok(invented)), None).status, "unverified");
        let refused = result("final", "I could not open the site.", "ERROR [APPROVAL_DENIED] browser_navigate: the site was not approved");
        assert_eq!(outcome_of("bitcoin price today", Ok(Ok(refused)), None).status, "refused");
        // The real shape of a refused approval: the run ends as "user_declined" with no tool steps.
        let declined = McpAgentResult {
            final_answer: "I stopped without doing it because you declined: opening https://www.google.com/search?q=bitcoin.".into(),
            steps: vec![],
            tool_rounds: 0,
            stopped_reason: "user_declined".into(),
            gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
        };
        let o = outcome_of("bitcoin price today", Ok(Ok(declined)), None);
        assert_eq!((o.status.as_str(), o.verified), ("refused", false));
        assert!(o.note.contains("opening https://www.google.com") && o.note.contains("Always allow"), "{}", o.note);
    }

    #[test]
    fn the_run_now_request_is_consumed_once() {
        request_run_now();
        assert!(FORCE.swap(false, Ordering::SeqCst));
        assert!(!FORCE.swap(false, Ordering::SeqCst));
        assert!(last_note().contains("requested"));
    }
}

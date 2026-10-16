use super::model::*;
use super::run::model::*;
use super::supervisor::ladder::{decide, Action, Situation};
use super::supervisor::{classify, ErrorClass, VerdictKind};
use serde_json::{json, Map};

#[test]
fn failures_are_named_by_what_they_say_and_unclear_ones_stay_unknown() {
    use ErrorClass::*;
    for (text, want) in [
        ("HTTP 429 Too Many Requests", RateLimit),
        ("the service returned status 401 unauthorized", Authentication),
        ("Your session has expired, log in again", Authentication),
        ("403 Forbidden", PermissionError),
        ("access denied by the folder policy", PermissionError),
        ("request timed out after 30s", Timeout),
        ("selector #post-button: element not found", WebsiteUiChange),
        ("the browser page crashed", BrowserError),
        ("No such file or directory (os error 2)", MissingFile),
        ("report.xlsx not found in the folder", MissingFile),
        ("error sending request: connection refused", Network),
        ("502 Bad Gateway", ExternalService),
        ("service unavailable, try later", ExternalService),
        ("HTTP 500 internal error", ApiError),
        ("missing: File label", InvalidInput),
        ("the model said something odd", Unknown),
        ("listening on port 14290", Unknown),
    ] {
        assert_eq!(classify(text), want, "{text}");
    }
}

#[test]
fn the_ladder_goes_retry_then_another_way_then_diagnose_then_ask_then_stop() {
    let mut s = Situation::first(ErrorClass::Network, 1);
    assert_eq!(decide(&s), Action::Retry { after_secs: 5 });
    s.attempts = 2;
    assert_eq!(decide(&s), Action::Retry { after_secs: 30 });
    s.attempts = 3;
    assert_eq!(decide(&s), Action::Retry { after_secs: 120 });
    s.attempts = 4;
    assert_eq!(decide(&s), Action::Diagnose, "no alternative registered yet");
    s.has_alternative = true;
    assert!(matches!(decide(&s), Action::Alternative { .. }));
    s.alternative_tried = true;
    assert_eq!(decide(&s), Action::Diagnose);
    s.diagnosed = true;
    assert_eq!(decide(&s), Action::AskUser);
    s.asked = true;
    assert_eq!(decide(&s), Action::StopSafely, "the ladder ends; it never loops");
}

#[test]
fn failures_that_cannot_fix_themselves_skip_the_retries() {
    for class in [ErrorClass::Authentication, ErrorClass::PermissionError] {
        assert_eq!(decide(&Situation::first(class, 1)), Action::AskUser, "{class:?}: a person has to fix it, no diagnosis will");
    }
    assert_eq!(decide(&Situation::first(ErrorClass::InvalidInput, 1)), Action::Diagnose);
    assert_eq!(decide(&Situation::first(ErrorClass::RateLimit, 1)), Action::Retry { after_secs: 60 });
}

#[test]
fn an_action_that_may_already_have_happened_is_never_retried() {
    let mut s = Situation::first(ErrorClass::Network, 1);
    s.outcome_unknown = true;
    assert_eq!(decide(&s), Action::AskUser);
    s.asked = true;
    assert_eq!(decide(&s), Action::StopSafely);
}

// ---- verdicts ---------------------------------------------------------------------------------------------------

fn def() -> FlowDef {
    serde_json::from_value(json!({
        "name": "Post",
        "nodes": [
            { "id": "start", "kind": "trigger" },
            { "id": "write", "kind": "text_generation", "name": "Write" },
            { "id": "publish", "kind": "social_media", "name": "Publish", "config": { "platform": "linkedin", "action": "publish" } }
        ],
        "edges": [{ "from": "start", "to": "write" }, { "from": "write", "to": "publish" }]
    }))
    .unwrap()
}

fn run(status: RunStatus, dry: bool) -> Run {
    Run { id: 1, flow_id: 1, owner_id: "o".into(), flow_version: 1, name: "Post".into(), status, trigger_kind: "schedule".into(), dry_run: dry, vars: Map::new(), payload: json!({}), created_at: 0, started_at: 1, finished_at: 2, error: String::new(), from_run: None, def: def() }
}

fn step(id: &str, status: NodeStatus, attempts: u32, outputs: serde_json::Value, error: &str) -> NodeRun {
    let mut n = NodeRun::pending(1, id);
    n.status = status;
    n.attempts = attempts;
    n.error = error.into();
    n.outputs = outputs.as_object().cloned().unwrap_or_default();
    n
}

#[test]
fn success_needs_proof_of_what_was_done_outside() {
    let d = def();
    let ok = [step("start", NodeStatus::Succeeded, 1, json!({}), ""), step("write", NodeStatus::Succeeded, 1, json!({ "text": "hi" }), ""), step("publish", NodeStatus::Succeeded, 1, json!({ "status": "published", "post_url": "https://x/1" }), "")];
    let v = super::supervisor::judge(&run(RunStatus::Succeeded, false), &ok, &d);
    assert_eq!(v.kind, VerdictKind::Success, "{v:?}");
    assert!(v.evidence.iter().any(|e| e.contains("https://x/1")));

    let no_proof = [step("start", NodeStatus::Succeeded, 1, json!({}), ""), step("write", NodeStatus::Succeeded, 1, json!({ "text": "hi" }), ""), step("publish", NodeStatus::Succeeded, 1, json!({}), "")];
    let v = super::supervisor::judge(&run(RunStatus::Succeeded, false), &no_proof, &d);
    assert_eq!(v.kind, VerdictKind::Unknown, "a publish with nothing to show for it is not a success: {v:?}");
    assert!(v.reason.contains("Publish"));
}

#[test]
fn a_step_that_needed_a_retry_is_recovered_not_plain_success() {
    let nodes = [step("start", NodeStatus::Succeeded, 1, json!({}), ""), step("write", NodeStatus::Succeeded, 3, json!({ "text": "hi" }), ""), step("publish", NodeStatus::Succeeded, 1, json!({ "post_url": "https://x/1" }), "")];
    let v = super::supervisor::judge(&run(RunStatus::Succeeded, false), &nodes, &def());
    assert_eq!(v.kind, VerdictKind::Recovered);
    assert!(v.reason.contains("Write") && v.reason.contains("attempt 3"));
}

#[test]
fn a_run_that_carried_past_a_failure_is_a_failure_with_its_class() {
    let nodes = [step("start", NodeStatus::Succeeded, 1, json!({}), ""), step("write", NodeStatus::Failed, 1, json!({ "ok": false }), "request timed out"), step("publish", NodeStatus::Skipped, 0, json!({}), "")];
    let v = super::supervisor::judge(&run(RunStatus::Succeeded, false), &nodes, &def());
    assert_eq!((v.kind, v.class), (VerdictKind::Failed, Some(ErrorClass::Timeout)));
}

#[test]
fn every_other_ending_says_what_it_is() {
    let d = def();
    let failed = [step("write", NodeStatus::Failed, 1, json!({}), "401 unauthorized")];
    let v = super::supervisor::judge(&run(RunStatus::Failed, false), &failed, &d);
    assert_eq!((v.kind, v.class), (VerdictKind::Failed, Some(ErrorClass::Authentication)));
    assert!(v.reason.contains("Write"));
    assert_eq!(super::supervisor::judge(&run(RunStatus::Waiting, false), &[], &d).kind, VerdictKind::Waiting);
    assert_eq!(super::supervisor::judge(&run(RunStatus::Running, false), &[], &d).kind, VerdictKind::Waiting);
    assert_eq!(super::supervisor::judge(&run(RunStatus::Interrupted, false), &[step("publish", NodeStatus::Interrupted, 1, json!({}), "")], &d).kind, VerdictKind::Unknown);
    assert_eq!(super::supervisor::judge(&run(RunStatus::Cancelled, false), &[], &d).kind, VerdictKind::Failed);
    let dry = super::supervisor::judge(&run(RunStatus::Succeeded, true), &[], &d);
    assert_eq!(dry.kind, VerdictKind::Unknown, "a dry run proves nothing: {dry:?}");
}

#[test]
fn the_log_never_says_a_sub_agent_was_started_for_a_failure_because_researching_one_is_not_built() {
    let line = crate::flows::run::engine::research_not_built("Post to LinkedIn", "HTTP 400 bad request");
    assert!(line.contains("is not built") && line.contains("nothing was started") && line.contains("the step failed"), "{line}");
    assert!(!line.contains("spawning") && !line.contains("sub-agent") && !line.contains("initiated"), "{line}");
    assert!(line.contains("Post to LinkedIn") && line.contains("HTTP 400 bad request"), "the step and its own error are in the line: {line}");
    // The engine reaches the rung only for a class that a plain retry may help (`ladder_action`), and the ladder asks for research only for a class that one cannot help:
    // the two never meet today, which is why the event was never seen. If they ever do, the engine must start a real research and this test is to be changed with it.
    for class in [ErrorClass::Network, ErrorClass::Timeout, ErrorClass::RateLimit, ErrorClass::ExternalService, ErrorClass::BrowserError] {
        assert!(class.transient());
        for failures in 1..12 {
            assert_ne!(decide(&Situation::first(class, failures)), Action::ResearchDocs, "{class:?} after {failures} failures");
        }
    }
}

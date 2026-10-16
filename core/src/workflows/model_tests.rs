use super::model::*;
use serde_json::json;

fn input() -> WorkflowInput {
    serde_json::from_value(json!({
        "name": "Morning report",
        "description": "Every day at 9",
        "enabled": true,
        "trigger": { "type": "schedule", "cron": "0 9 * * *" },
        "steps": [
            { "id": "fetch", "type": "tool", "tool": "web.get", "arguments": { "url": "https://example.com" }, "retries": 2 },
            { "id": "note", "type": "set", "value": "Got: {{steps.fetch.output}}", "only_if": "{{steps.fetch.ok}}" },
            { "id": "sum", "type": "agent", "goal": "Summarise {{steps.note.output}}", "max_steps": 6, "on_failure": "continue" }
        ]
    }))
    .unwrap()
}

#[test]
fn what_the_ui_sends_parses_and_validates() {
    let i = input();
    i.validate().unwrap();
    assert_eq!(i.trigger.kind(), "schedule");
    assert_eq!(i.steps.len(), 3);
    assert_eq!(i.steps[0].retries, 2);
    assert_eq!(i.steps[0].retry_delay_secs, 5, "default retry delay");
    assert_eq!(i.steps[2].on_failure, OnFailure::Continue);
    assert!(!i.catch_up);
    // Round trip: what is stored is what is read back.
    let back: WorkflowInput = serde_json::from_str(&serde_json::to_string(&i).unwrap()).unwrap();
    assert_eq!(back, i);
    for t in [json!({ "type": "manual" }), json!({ "type": "startup" }), json!({ "type": "webhook" }), json!({ "type": "interval", "every_secs": 300 })] {
        let mut j = input();
        j.trigger = serde_json::from_value(t).unwrap();
        j.validate().unwrap();
    }
}

fn err_of(edit: impl FnOnce(&mut WorkflowInput)) -> String {
    let mut i = input();
    edit(&mut i);
    i.validate().unwrap_err()
}

#[test]
fn mistakes_are_refused_with_a_plain_reason() {
    assert!(err_of(|i| i.name = "  ".into()).contains("name"));
    assert!(err_of(|i| i.name = "x".repeat(81)).contains("name"));
    assert!(err_of(|i| i.steps.clear()).contains("at least one step"));
    assert!(err_of(|i| i.steps = (0..21).map(|n| { let mut s = input().steps[1].clone(); s.id = format!("s{n}"); s }).collect()).contains("at most 20"));
    assert!(err_of(|i| i.steps[1].id = "fetch".into()).contains("already uses"));
    assert!(err_of(|i| i.steps[0].id = "Has Space".into()).contains("id"));
    assert!(err_of(|i| i.steps[0].retries = 6).contains("retries"));
    assert!(err_of(|i| i.steps[1].only_if = Some("{{steps.fetch.ok}} ==".into())).contains("condition"), "a condition that cannot be read is refused when saved");
    assert!(err_of(|i| i.trigger = Trigger::Schedule { cron: "61 * * * *".into() }).contains("schedule"));
    assert!(err_of(|i| i.trigger = Trigger::Interval { every_secs: 10 }).contains("at least 60"));
    assert!(err_of(|i| i.steps[0].kind = StepKind::Tool { tool: "rm -rf".into(), arguments: json!({}) }).contains("tool name"));
    assert!(err_of(|i| i.steps[0].kind = StepKind::Tool { tool: "fs.read".into(), arguments: json!([1]) }).contains("object"));
    assert!(err_of(|i| i.steps[2].kind = StepKind::Agent { goal: "".into(), max_steps: 5 }).contains("goal"));
    assert!(err_of(|i| i.steps[2].kind = StepKind::Agent { goal: "go".into(), max_steps: 99 }).contains("limit"));
}

#[test]
fn the_agent_step_limit_is_the_number_of_rounds_the_agent_really_takes() {
    let most = crate::modules::browser_agent::AGENT_MAX_ROUNDS;
    assert_eq!(most, 12);
    let with = |n: u32| {
        let mut i = input();
        i.steps[2].kind = StepKind::Agent { goal: "go".into(), max_steps: n };
        i.validate()
    };
    assert!(with(most).is_ok(), "12 is accepted");
    assert!(with(1).is_ok());
    let refused = with(most + 1).unwrap_err();
    assert!(refused.contains("1 to 12"), "13 is refused with the real limit: {refused}");
    assert!(with(0).is_err());
}

#[test]
fn secrets_are_never_stored_in_a_workflow() {
    let e = err_of(|i| i.steps[0].kind = StepKind::Tool { tool: "web.get".into(), arguments: json!({ "url": "https://x.example", "headers": { "Authorization": "Bearer abc123" } }) });
    assert!(e.contains("headers.Authorization") && e.contains("plain text"), "{e}");
    assert!(err_of(|i| i.steps[0].kind = StepKind::Tool { tool: "web.get".into(), arguments: json!({ "password": "hunter2" }) }).contains("arguments.password"));
    assert!(err_of(|i| i.steps[0].kind = StepKind::Tool { tool: "web.get".into(), arguments: json!({ "note": "key is sk-live-abcdef123456" }) }).contains("arguments.note"));
    assert!(err_of(|i| i.steps[0].kind = StepKind::Tool { tool: "web.get".into(), arguments: json!({ "list": ["ghp_abcdefghijklmnop"] }) }).contains("list[0]"));
    assert!(err_of(|i| i.steps[2].kind = StepKind::Agent { goal: "log in with password=hunter2".into(), max_steps: 5 }).contains("password or token"));
    // A reference to a value is fine: it is not the value.
    let mut ok = input();
    ok.steps[0].kind = StepKind::Tool { tool: "web.get".into(), arguments: json!({ "url": "https://x.example/?token={{trigger.payload.t}}", "token": "{{trigger.payload.t}}" }) };
    ok.validate().unwrap();
}

#[test]
fn owner_is_local_for_now() {
    assert_eq!(current_owner(), "local");
    assert_eq!(RunStatus::parse(RunStatus::Partial.as_str()), RunStatus::Partial);
    assert_eq!(RunStatus::parse("garbage"), RunStatus::Interrupted);
}

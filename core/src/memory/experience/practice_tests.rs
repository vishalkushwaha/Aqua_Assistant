//! Tests for auto-generated practice tasks: what may be generated, when it may run, and what it does.

use super::model::*;
use super::practice::*;
use super::service::LearningService;
use super::settings::LearningSettings;
use super::vector::{embedding_text, EmbeddingProvider, HashEmbedding};
use std::sync::Arc;

fn svc(name: &str) -> Arc<LearningService> {
    let d = std::env::temp_dir().join(format!("aqua_practice_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    LearningService::manual(&d, Arc::new(HashEmbedding))
}

fn enable(s: &LearningService, f: impl FnOnce(&mut LearningSettings)) {
    let mut st = s.settings();
    st.auto_practice_enabled = true;
    f(&mut st);
    s.update_settings(&st).unwrap();
}

/// A verified live-data experience the agent could practise around.
fn seed_experience(s: &LearningService, problem: &str) -> i64 {
    let mut e = Experience::candidate(problem, "live_data");
    e.approach = "search then read the page".into();
    e.important_steps = vec!["browser_navigate to a Google search".into()];
    e.tools_used = vec!["browser_navigate".into()];
    e.verified = true;
    e.success = true;
    e.verification_method = VerificationMethod::PageEvidence;
    e.status = ExperienceStatus::Verified;
    e.confidence = 0.5;
    e.last_verified_at = now_secs();
    e.embedding_model = "aqua-hash-384".into();
    e.embedding_version = 1;
    s.store().insert(&e, &HashEmbedding.embed(&embedding_text(&e)).unwrap()).unwrap()
}

fn idle_gate(now: i64) -> Gate {
    Gate { now, last_user_activity: now - 3600, user_active: false, force: false }
}

fn verified_outcome() -> PracticeOutcome {
    PracticeOutcome { status: "verified".into(), verified: true, steps: 3, note: "figures found on the page".into() }
}

#[test]
fn every_template_and_entity_is_a_safe_read_only_request() {
    // Guards future edits to the lists: nothing added later may produce an unsafe goal.
    for f in FAMILIES {
        for t in f.templates {
            for e in f.entities {
                let goal = t.replace("{e}", e);
                assert!(is_safe_practice_goal(&goal), "family {} produced an unsafe or non-live goal: {goal:?}", f.name);
            }
        }
    }
}

#[test]
fn the_filter_refuses_actions_accounts_addresses_secrets_and_non_lookups() {
    for bad in [
        "send an email with today's stock market condition",
        "buy bitcoin at today's price",
        "open the browser and show today's weather",
        "what is my account balance today",
        "check https://example.com for the latest news",
        "post the latest news to my channel",
        "delete today's news file",
        "what is the weather in Pune today password=hunter2",
        "run a command to get today's price",
        "tell me the price of gold today; rm -rf /",
        "explain what a stock market is",
        "write a poem",
        "hi",
    ] {
        assert!(!is_safe_practice_goal(bad), "should be refused: {bad:?}");
    }
    assert!(is_safe_practice_goal("what is the weather in Pune today"));
    assert!(is_safe_practice_goal("tell me today stock market condition"));
    assert!(is_safe_practice_goal("top 3 news headlines today"));
}

#[test]
fn nothing_is_practised_until_the_agent_has_verified_something_to_practise_around() {
    assert!(generate(&[], &[], 1).is_none());
    assert!(generate(&["explain recursion".to_string()], &[], 1).is_none(), "an unrelated kind is not a reason to practise");
    let problems = vec!["bitcoin price today".to_string()];
    let task = generate(&problems, &[], 1).expect("a crypto experience makes crypto practice possible");
    assert!(is_safe_practice_goal(&task.goal), "{task:?}");
}

#[test]
fn generated_tasks_stay_in_the_kinds_you_use_and_are_new() {
    let problems = vec!["bitcoin price today".to_string(), "tell me today stock market condition".to_string()];
    for seed in 0..40u64 {
        let t = generate(&problems, &[], seed).unwrap();
        assert!(matches!(t.family.as_str(), "crypto_price" | "stock_market"), "weather was never used, so no weather practice: {t:?}");
        assert!(!problems.iter().any(|p| p.eq_ignore_ascii_case(&t.goal)), "never an exact repeat of a known task: {t:?}");
        assert!(is_safe_practice_goal(&t.goal));
    }
    // Both ways of practising appear: new tasks and rewordings.
    let sources: std::collections::BTreeSet<String> = (0..12u64).filter_map(|s| generate(&problems, &[], s)).map(|t| t.source).collect();
    assert_eq!(sources.into_iter().collect::<Vec<_>>(), vec!["paraphrase", "variation"]);
    // Same seed, same task.
    assert_eq!(generate(&problems, &[], 7), generate(&problems, &[], 7));
}

#[test]
fn a_practised_goal_is_not_generated_again_until_the_options_run_out() {
    let problems = vec!["bitcoin price today".to_string()];
    let mut done: Vec<String> = Vec::new();
    for seed in 0..200u64 {
        match generate(&problems, &done, seed) {
            Some(t) => {
                assert!(!done.iter().any(|d| d.eq_ignore_ascii_case(&t.goal)), "repeated: {}", t.goal);
                done.push(t.goal);
            }
            None => break,
        }
    }
    assert!(done.len() >= 8, "there is a fair amount of variety: {done:?}");
    assert!(generate(&problems, &done, 1).is_none(), "and when it is all done, it stops instead of repeating");
}

#[test]
fn the_gates_decide_when_practice_may_start() {
    let now = 1_000_000_000;
    let mut st = LearningSettings::default();
    let gate = idle_gate(now);
    assert_eq!(check(&st, 0, 0, &gate), Err(Skip::Disabled), "off by default");
    st.auto_practice_enabled = true;
    assert_eq!(check(&st, 0, 0, &gate), Ok(()));
    st.learning_enabled = false;
    assert_eq!(check(&st, 0, 0, &gate), Err(Skip::Disabled), "the master switch turns practice off too");
    st.learning_enabled = true;

    assert_eq!(check(&st, 0, 0, &Gate { user_active: true, ..gate }), Err(Skip::UserActive));
    assert!(matches!(check(&st, 0, 0, &Gate { last_user_activity: now - 120, ..gate }), Err(Skip::NotIdle { idle_mins: 2, needs_mins: 10 })));
    assert_eq!(check(&st, 3, 0, &gate), Err(Skip::DailyLimit { limit: 3 }));
    assert!(matches!(check(&st, 1, now - 600, &gate), Err(Skip::TooSoon { mins_left: 50 })));
    assert_eq!(check(&st, 1, now - 4000, &gate), Ok(()));
    // Nobody has used the agent since start-up: that counts as idle.
    assert_eq!(check(&st, 0, 0, &Gate { last_user_activity: 0, ..gate }), Ok(()));

    // "Run one now" skips the idle wait and the gap, and nothing else.
    let forced = Gate { force: true, last_user_activity: now - 30, ..gate };
    assert_eq!(check(&st, 1, now - 60, &forced), Ok(()));
    assert_eq!(check(&st, 3, 0, &forced), Err(Skip::DailyLimit { limit: 3 }), "the daily budget still applies");
    assert_eq!(check(&st, 0, 0, &Gate { user_active: true, ..forced }), Err(Skip::UserActive), "it never competes with a user's task");
    let mut off = st.clone();
    off.auto_practice_enabled = false;
    assert_eq!(check(&off, 0, 0, &forced), Err(Skip::Disabled));
}

#[test]
fn a_tick_generates_runs_records_and_then_waits() {
    let s = svc("tick");
    seed_experience(&s, "tell me today stock market condition");
    enable(&s, |_| {});
    let now = now_secs();
    let mut ran_goal = String::new();
    let result = tick(&s, &idle_gate(now), |task, settings| {
        ran_goal = task.goal.clone();
        assert!(settings.practice_max_steps >= 2);
        verified_outcome()
    });
    let TickResult::Ran { id, task, outcome } = result else { panic!("{result:?}") };
    assert_eq!(ran_goal, task.goal);
    assert!(outcome.verified && task.family == "stock_market", "{task:?}");
    let rows = s.store().practice_recent(5).unwrap();
    assert_eq!((rows.len(), rows[0].id, rows[0].status.as_str(), rows[0].verified, rows[0].steps), (1, id, "verified", true, 3));
    let stats = s.stats().unwrap();
    assert_eq!((stats.practice_runs, stats.practice_verified), (1, 1));
    // Right after, the minimum gap applies.
    assert!(matches!(tick(&s, &idle_gate(now + 30), |_, _| panic!("must not run")), TickResult::Skipped(Skip::TooSoon { .. })));
    // Later, a different task is generated: the first is never repeated.
    let later = now + 2 * 3600;
    let TickResult::Ran { task: second, .. } = tick(&s, &idle_gate(later), |_, _| verified_outcome()) else { panic!() };
    assert_ne!(second.goal, task.goal);
    let events: Vec<String> = s.store().recent_events(20).unwrap().into_iter().map(|e| e.event).collect();
    assert!(events.contains(&"practice_task_started".to_string()) && events.contains(&"practice_task_completed".to_string()));
}

#[test]
fn practice_respects_the_daily_limit_and_the_switches() {
    let s = svc("limits");
    seed_experience(&s, "bitcoin price today");
    // Off by default: nothing runs and nothing is recorded.
    assert_eq!(tick(&s, &idle_gate(now_secs()), |_, _| panic!("must not run")), TickResult::Skipped(Skip::Disabled));
    assert!(s.store().practice_recent(5).unwrap().is_empty());
    enable(&s, |st| {
        st.practice_max_per_day = 2;
        st.practice_min_interval_mins = 5;
    });
    let mut now = now_secs();
    let mut ran = 0;
    for _ in 0..6 {
        now += 3600;
        if let TickResult::Ran { .. } = tick(&s, &idle_gate(now), |_, _| verified_outcome()) {
            ran += 1;
        }
    }
    assert_eq!(ran, 2, "at most the daily limit within 24 hours");
    // Turning learning off stops it at once.
    let mut st = s.settings();
    st.learning_enabled = false;
    s.update_settings(&st).unwrap();
    assert_eq!(tick(&s, &idle_gate(now + 90 * 3600), |_, _| panic!("must not run")), TickResult::Skipped(Skip::Disabled));
}

#[test]
fn nothing_to_practise_and_a_failing_or_panicking_run_are_handled() {
    let s = svc("edge");
    enable(&s, |_| {});
    assert_eq!(tick(&s, &idle_gate(now_secs()), |_, _| panic!("must not run")), TickResult::Skipped(Skip::NothingToPractice));
    seed_experience(&s, "bitcoin price today");
    let now = now_secs();
    let TickResult::Ran { outcome, .. } = tick(&s, &idle_gate(now), |_, _| PracticeOutcome { status: "unverified".into(), verified: false, steps: 1, note: "figures were not on the page".into() }) else { panic!() };
    assert!(!outcome.verified);
    let TickResult::Ran { outcome, .. } = tick(&s, &idle_gate(now + 7200), |_, _| panic!("boom")) else { panic!() };
    assert_eq!(outcome.status, "failed", "a panic in the run is contained");
    let rows = s.store().practice_recent(5).unwrap();
    assert_eq!((rows[0].status.as_str(), rows[1].status.as_str()), ("failed", "unverified"));
    let stats = s.stats().unwrap();
    assert_eq!((stats.practice_runs, stats.practice_verified), (2, 0));
}

#[test]
fn a_refused_practice_is_not_retried_for_hours_unless_you_ask() {
    let s = svc("refused");
    seed_experience(&s, "bitcoin price today");
    enable(&s, |st| st.practice_min_interval_mins = 5);
    let now = now_secs();
    let refused = PracticeOutcome { status: "refused".into(), verified: false, steps: 0, note: "needed an approval it cannot give".into() };
    assert!(matches!(tick(&s, &idle_gate(now), |_, _| refused), TickResult::Ran { .. }));
    // An hour later it does not ask again: it would only be refused again.
    let later = tick(&s, &idle_gate(now + 3600), |_, _| panic!("must not run"));
    let TickResult::Skipped(Skip::NeedsApproval { hours_left }) = later else { panic!("{later:?}") };
    assert!((4..=6).contains(&hours_left), "{hours_left}");
    assert!(status(&s, &idle_gate(now + 3600)).waiting_for.contains("Always allow"));
    // "Run one now" means the person just did something about it, so it tries.
    let forced = Gate { force: true, ..idle_gate(now + 3600) };
    assert!(matches!(tick(&s, &forced, |_, _| verified_outcome()), TickResult::Ran { .. }));
    // And after the back-off it resumes by itself (the next task was verified, so nothing blocks).
    assert!(matches!(tick(&s, &idle_gate(now + 8 * 3600), |_, _| verified_outcome()), TickResult::Ran { .. }));
}

#[test]
fn status_explains_what_practice_is_waiting_for() {
    let s = svc("status");
    seed_experience(&s, "bitcoin price today");
    let now = now_secs();
    let st = status(&s, &idle_gate(now));
    assert!(!st.enabled && st.waiting_for.contains("off"), "{st:?}");
    enable(&s, |_| {});
    assert!(status(&s, &idle_gate(now)).waiting_for.starts_with("ready"));
    assert!(status(&s, &Gate { user_active: true, ..idle_gate(now) }).waiting_for.contains("busy"));
    assert!(status(&s, &Gate { last_user_activity: now - 60, ..idle_gate(now) }).waiting_for.contains("idle minutes"));
    tick(&s, &idle_gate(now), |_, _| verified_outcome());
    let after = status(&s, &idle_gate(now + 60));
    assert_eq!((after.started_last_24h, after.recent.len()), (1, 1));
    assert!(after.waiting_for.contains("next practice in") && after.next_allowed_at > now);
}

#[test]
fn a_crash_mid_practice_is_marked_interrupted_on_start_up() {
    let s = svc("recover");
    let id = s.store().practice_start("bitcoin price today", "crypto_price", "variation").unwrap();
    recover(&s);
    let rows = s.store().practice_recent(5).unwrap();
    assert_eq!((rows[0].id, rows[0].status.as_str()), (id, "interrupted"));
}

#[test]
fn practice_settings_are_bounded_and_off_by_default() {
    let d = LearningSettings::default();
    assert!(!d.auto_practice_enabled && !d.may_practice());
    let mut wild = d.clone();
    wild.practice_max_per_day = 10_000;
    wild.practice_min_interval_mins = 0;
    wild.practice_idle_mins = 0;
    wild.practice_max_steps = 500;
    let c = wild.clamped();
    assert_eq!((c.practice_max_per_day, c.practice_min_interval_mins, c.practice_idle_mins, c.practice_max_steps), (24, 5, 1, 10));
    // Settings saved before this feature existed still load, with practice off.
    let old: LearningSettings = serde_json::from_str(r#"{"learning_enabled": true, "max_attempts": 2}"#).unwrap();
    assert!(!old.auto_practice_enabled && old.max_attempts == 2);
}

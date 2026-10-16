//! Tests for the learning pipeline.

use super::learner::*;
use super::model::*;
use super::settings::LearningSettings;
use super::store::*;
use super::vector::{embedding_text, EmbeddingProvider, HashEmbedding};

fn store(name: &str) -> ExperienceStore {
    let d = std::env::temp_dir().join(format!("aqua_learn_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    ExperienceStore::new(&d)
}

fn record() -> TaskRecord {
    TaskRecord {
        task_id: "t1".into(),
        goal: "tell me today stock market condition".into(),
        task_type: "live_data".into(),
        technologies: vec!["browser".into()],
        steps: vec!["browser_navigate a google search for the topic".into(), "browser_snapshot read the results".into()],
        tools_used: vec!["browser_navigate".into(), "browser_snapshot".into()],
        constraints: vec!["quote only figures that are on the page".into()],
        result_summary: "answered with index levels quoted from the page".into(),
        finished: true,
        verification: Some(Verification::new(true, VerificationMethod::PageEvidence, "every quoted figure was found on the page")),
        tool_calls: 2,
        ..Default::default()
    }
}

fn learn_it(s: &ExperienceStore, t: &TaskRecord) -> LearningOutcome {
    learn(s, &HashEmbedding, &LearningSettings::default(), t).unwrap()
}

fn all(s: &ExperienceStore) -> Vec<Experience> {
    s.list(&ListFilter::default()).unwrap()
}

#[test]
fn a_verified_successful_task_becomes_a_low_confidence_experience() {
    let s = store("create");
    let out = learn_it(&s, &record());
    let LearningOutcome::Created(id) = out else { panic!("{out:?}") };
    let e = s.get(id).unwrap().unwrap();
    assert_eq!(e.status, ExperienceStatus::Verified);
    assert!(e.verified && e.success && e.verification_method == VerificationMethod::PageEvidence);
    assert!(e.confidence > 0.3 && e.confidence <= 0.6, "new experiences start modest: {}", e.confidence);
    assert!(e.quality_score >= MIN_QUALITY_TO_STORE);
    assert_eq!(e.tools_used, vec!["browser_navigate", "browser_snapshot"]);
    assert_eq!(e.experience_type, ExperienceType::Solution);
    assert_eq!((e.embedding_model.as_str(), e.embedding_version), ("aqua-hash-384", 1));
    let st = s.stats().unwrap();
    assert_eq!((st.tasks_analyzed, st.tasks_successful, st.experiences_created), (1, 1, 1));
    assert!(st.last_success_at > 0);
}

#[test]
fn unverified_or_failed_tasks_are_never_stored() {
    let s = store("reject");
    let mut claim = record();
    claim.verification = Some(Verification::new(true, VerificationMethod::ModelClaim, "the model says it worked"));
    assert!(matches!(learn_it(&s, &claim), LearningOutcome::Rejected(r) if r.contains("not a verified success")));
    let mut failed = record();
    failed.verification = Some(Verification::new(false, VerificationMethod::PageEvidence, "figures not on the page"));
    assert!(matches!(learn_it(&s, &failed), LearningOutcome::Rejected(_)));
    let mut unfinished = record();
    unfinished.finished = false;
    assert!(matches!(learn_it(&s, &unfinished), LearningOutcome::Rejected(_)));
    let mut none = record();
    none.verification = None;
    assert!(matches!(learn_it(&s, &none), LearningOutcome::Rejected(_)));
    assert!(all(&s).is_empty());
    let st = s.stats().unwrap();
    assert_eq!((st.tasks_analyzed, st.tasks_failed, st.experiences_rejected), (4, 4, 4));
}

#[test]
fn trivial_tasks_are_not_worth_remembering() {
    let s = store("trivial");
    let mut no_tools = record();
    no_tools.tools_used.clear();
    no_tools.steps.clear();
    assert!(matches!(learn_it(&s, &no_tools), LearningOutcome::Rejected(r) if r.contains("no reusable steps")));
    let mut short = record();
    short.goal = "hi".into();
    assert!(matches!(learn_it(&s, &short), LearningOutcome::Rejected(r) if r.contains("too short")));
    assert!(all(&s).is_empty());
}

#[test]
fn the_same_solution_twice_updates_one_experience_instead_of_duplicating() {
    let s = store("dup");
    let LearningOutcome::Created(id) = learn_it(&s, &record()) else { panic!() };
    let first = s.get(id).unwrap().unwrap();
    let mut again = record();
    again.task_id = "t2".into();
    again.goal = "tell me today's stock market condition".into();
    assert_eq!(learn_it(&s, &again), LearningOutcome::Updated(id));
    assert_eq!(all(&s).len(), 1, "no duplicate was created");
    let second = s.get(id).unwrap().unwrap();
    assert!(second.confidence > first.confidence, "seen again: a little more confidence");
    assert!(second.confidence <= 0.95);
    let st = s.stats().unwrap();
    assert_eq!((st.experiences_created, st.experiences_updated, st.duplicates_prevented), (1, 1, 1));
}

#[test]
fn a_better_solution_improves_the_existing_experience_and_keeps_its_history() {
    let s = store("better");
    let mut weak = record();
    weak.verification = Some(Verification::new(true, VerificationMethod::ToolResult, "the tool returned a result"));
    weak.steps = vec!["browser_navigate to a page".into(), "browser_snapshot".into()];
    let LearningOutcome::Created(id) = learn_it(&s, &weak) else { panic!() };
    let before = s.get(id).unwrap().unwrap();
    assert_eq!((before.verification_method, before.experience_version), (VerificationMethod::ToolResult, 1));

    let mut strong = record();
    strong.task_id = "t2".into();
    strong.verification = Some(Verification::new(true, VerificationMethod::Tests, "all tests passed"));
    strong.steps = vec!["browser_navigate a google search".into(), "browser_snapshot read the results".into(), "quote figures found on the page".into()];
    assert_eq!(learn_it(&s, &strong), LearningOutcome::Updated(id));
    let after = s.get(id).unwrap().unwrap();
    assert_eq!(after.verification_method, VerificationMethod::Tests, "stronger evidence replaces weaker");
    assert_eq!(after.experience_version, 2);
    assert!(after.approach.contains("quote figures found on the page"));
    assert!(after.quality_score >= before.quality_score);
    let events = s.recent_events(20).unwrap();
    let updated = events.iter().find(|e| e.event == "experience_updated").expect("update was logged");
    assert!(updated.detail["previous_approach"].as_str().unwrap().contains("browser_navigate to a page"), "the replaced approach is kept in the log");
}

#[test]
fn a_failure_that_was_then_corrected_is_captured_as_an_error_fix() {
    let s = store("errorfix");
    let mut t = record();
    t.failed_attempts = vec![FailedAttempt { approach: "answered from the model's memory".into(), reason: "figures were not on the page".into() }];
    let LearningOutcome::Created(id) = learn_it(&s, &t) else { panic!() };
    let e = s.get(id).unwrap().unwrap();
    assert_eq!(e.experience_type, ExperienceType::ErrorFix);
    assert_eq!(e.failed_approach.as_deref(), Some("answered from the model's memory"));
    assert_eq!(e.failure_reason.as_deref(), Some("figures were not on the page"));
    assert!(e.correction.as_deref().unwrap().contains("browser_navigate"));
    // The same failure without a verified correction is not stored at all.
    let mut unfixed = t.clone();
    unfixed.verification = None;
    unfixed.goal = "a completely different unfixed task about databases".into();
    assert!(matches!(learn_it(&s, &unfixed), LearningOutcome::Rejected(_)));
    assert_eq!(all(&s).len(), 1);
}

#[test]
fn secrets_in_a_task_never_reach_the_database() {
    let s = store("secrets");
    let mut t = record();
    t.goal = "log in to the portal with password=hunter2 and read the dashboard totals".into();
    t.steps = vec!["browser_navigate https://admin:s3cretpw@portal.example.com/login".into(), "browser_type token ghp_abcdefghijklmnopqrstuvwxyz0123456789".into()];
    t.result_summary = "signed in as jane.doe@example.com".into();
    let LearningOutcome::Created(id) = learn_it(&s, &t) else { panic!() };
    let stored = serde_json::to_string(&s.get(id).unwrap().unwrap()).unwrap();
    for leaked in ["hunter2", "s3cretpw", "ghp_abcdef", "jane.doe"] {
        assert!(!stored.contains(leaked), "{leaked} leaked into: {stored}");
    }
    assert!(s.recent_events(20).unwrap().iter().any(|e| e.event == "experience_sanitized"));
    // A task that is mostly credentials is refused outright.
    let mut dump = record();
    dump.goal = "password=a token=b secret=c apikey=d".into();
    assert!(matches!(learn_it(&s, &dump), LearningOutcome::Rejected(r) if r.contains("sensitive")));
}

#[test]
fn reuse_outcomes_move_confidence_up_on_success_and_down_on_failure_only_when_definite() {
    let s = store("reuse");
    let LearningOutcome::Created(id) = learn_it(&s, &record()) else { panic!() };
    let base = s.get(id).unwrap().unwrap().confidence;
    let usage = s.record_usage(id, "tell me today stock market condition", 0.4).unwrap();
    let used = UsedExperience { experience_id: id, usage_id: Some(usage) };

    // Success with verification credits it.
    let mut win = record();
    win.task_id = "w".into();
    win.used_experiences = vec![used];
    learn_it(&s, &win);
    let e = s.get(id).unwrap().unwrap();
    assert_eq!((e.reuse_count, e.successful_reuse_count, e.failed_reuse_count), (1, 1, 0));
    assert!(e.confidence > base);
    assert_eq!(s.usage_for(id, 5).unwrap()[0].success, Some(true));

    // A finished run that could not be verified is neither a win nor a loss.
    let mut unsure = record();
    unsure.verification = Some(Verification::new(true, VerificationMethod::ModelClaim, "the model says so"));
    unsure.used_experiences = vec![used];
    learn_it(&s, &unsure);
    assert_eq!(s.get(id).unwrap().unwrap().reuse_count, 1, "an unverifiable outcome is not counted");

    // A definite failure debits it, and repeated failure deprecates it.
    let before = s.get(id).unwrap().unwrap().confidence;
    let mut loss = record();
    loss.verification = Some(Verification::new(false, VerificationMethod::PageEvidence, "quoted figures were not on the page"));
    loss.used_experiences = vec![used];
    for _ in 0..4 {
        learn_it(&s, &loss);
    }
    let e = s.get(id).unwrap().unwrap();
    assert!(e.confidence < before);
    assert_eq!(e.status, ExperienceStatus::Deprecated, "{:?} after {} failed reuses", e.status, e.failed_reuse_count);
    assert!(s.recent_events(50).unwrap().iter().any(|e| e.event == "experience_deprecated"));
}

#[test]
fn a_practice_run_can_refresh_or_debit_an_experience_but_never_promote_it() {
    let s = store("practice_credit");
    let LearningOutcome::Created(id) = learn_it(&s, &record()) else { panic!() };
    let usage = s.record_usage(id, "what is the stock market condition today", 0.5).unwrap();
    let used = UsedExperience { experience_id: id, usage_id: Some(usage) };
    let before = s.get(id).unwrap().unwrap();
    let old = now_secs() - 5 * 86_400;
    let mut aged = before.clone();
    aged.last_verified_at = old;
    s.update(&aged, None).unwrap();

    // A passing practice run: the experience is fresher, nothing else moves.
    let mut passed = record();
    passed.task_id = "practice-run-1".into();
    passed.goal = "what is the stock market condition today".into();
    passed.used_experiences = vec![used];
    learn_it(&s, &passed);
    let after = s.get(id).unwrap().unwrap();
    assert!(after.last_verified_at > old, "a passed re-check refreshes last-verified");
    assert_eq!((after.reuse_count, after.successful_reuse_count, after.failed_reuse_count), (0, 0, 0), "and does not count as reuse");
    assert_eq!(after.confidence, before.confidence, "and adds no confidence");
    assert_eq!(after.status, ExperienceStatus::Verified);
    assert_eq!(all(&s).len(), 1, "finding the same solution again (a merge) is not new evidence either");
    assert_eq!(s.usage_for(id, 5).unwrap()[0].success, Some(true));

    // Even many passing practice runs cannot make it trusted.
    for i in 0..6 {
        let mut again = passed.clone();
        again.task_id = format!("practice-run-{}", i + 2);
        again.used_experiences = vec![used];
        learn_it(&s, &again);
    }
    let still = s.get(id).unwrap().unwrap();
    assert_eq!((still.status, still.confidence), (ExperienceStatus::Verified, before.confidence), "the agent cannot promote its own memory by practising");

    // A failed practice run is real evidence against it.
    let mut failed = record();
    failed.task_id = "practice-run-99".into();
    failed.verification = Some(Verification::new(false, VerificationMethod::PageEvidence, "quoted figures were not on the page"));
    failed.used_experiences = vec![used];
    learn_it(&s, &failed);
    let hurt = s.get(id).unwrap().unwrap();
    assert_eq!((hurt.failed_reuse_count, hurt.reuse_count), (1, 1));
    assert!(hurt.confidence < before.confidence);

    // A person's successful use does earn it.
    let mut user_run = record();
    user_run.task_id = "run-7".into();
    user_run.used_experiences = vec![used];
    learn_it(&s, &user_run);
    assert_eq!(s.get(id).unwrap().unwrap().successful_reuse_count, 1);
}

#[test]
fn duplicates_are_only_merged_within_the_same_project() {
    let s = store("scopes");
    let mut a = record();
    a.project_key = "web:google.com".into();
    let LearningOutcome::Created(ida) = learn_it(&s, &a) else { panic!() };
    let mut b = record();
    b.task_id = "t2".into();
    b.project_key = "web:other.test".into();
    let LearningOutcome::Created(idb) = learn_it(&s, &b) else { panic!("a different project must not be merged into the first") };
    assert_ne!(ida, idb);
    assert_eq!(all(&s).len(), 2);
    assert!(all(&s).iter().all(|e| e.scope == ExperienceScope::Project));
}

#[test]
fn the_learner_reports_an_infrastructure_failure_so_the_job_can_retry() {
    struct Down;
    impl EmbeddingProvider for Down {
        fn model(&self) -> &str { "down" }
        fn version(&self) -> u32 { 1 }
        fn dim(&self) -> usize { 384 }
        fn embed(&self, _: &str) -> Result<Vec<f32>, String> { Err("embedding service unreachable".into()) }
    }
    let s = store("infra");
    let err = learn(&s, &Down, &LearningSettings::default(), &record()).unwrap_err();
    assert!(err.contains("unreachable"));
    assert!(all(&s).is_empty(), "nothing half-written");
    let _ = embedding_text(&Experience::candidate("x", "y"));
}

#[test]
fn different_problems_solved_with_the_same_tools_stay_separate_experiences() {
    let s = store("separate");
    let LearningOutcome::Created(a) = learn_it(&s, &record()) else { panic!() };
    let mut weather = record();
    weather.task_id = "t2".into();
    weather.goal = "what is the weather forecast for pune tomorrow".into();
    let LearningOutcome::Created(b) = learn_it(&s, &weather) else { panic!("an unrelated problem must not be merged into the market experience") };
    assert_ne!(a, b);
    let mut calc = record();
    calc.task_id = "t3".into();
    calc.goal = "open the calculator and multiply twelve by eight".into();
    calc.task_type = "desktop".into();
    let LearningOutcome::Created(c) = learn_it(&s, &calc) else { panic!() };
    assert!(c != a && c != b);
    assert_eq!(all(&s).len(), 3);
    // A reworded version of the first still merges into it.
    let mut again = record();
    again.task_id = "t4".into();
    again.goal = "what is the stock market condition today".into();
    assert_eq!(learn_it(&s, &again), LearningOutcome::Updated(a));
    assert_eq!(all(&s).len(), 3);
}

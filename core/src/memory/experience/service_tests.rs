//! Tests for the learning service: the full loop, the switches, failure isolation and management.

use super::learner::*;
use super::model::*;
use super::retrieval::{analyze_task, RetrievalResult};
use super::service::*;
use super::store::*;
use super::vector::{embedding_text, EmbeddingProvider, HashEmbedding};
use std::sync::Arc;

fn root(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("aqua_svc_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn svc(name: &str) -> Arc<LearningService> {
    LearningService::manual(&root(name), Arc::new(HashEmbedding))
}

fn tools() -> Vec<String> {
    ["browser_navigate", "browser_snapshot"].iter().map(|s| s.to_string()).collect()
}

fn record(goal: &str) -> TaskRecord {
    TaskRecord {
        task_id: "t".into(),
        goal: goal.into(),
        task_type: "live_data".into(),
        technologies: vec!["browser".into()],
        steps: vec!["browser_navigate a google search for the topic".into(), "browser_snapshot read the results".into()],
        tools_used: tools(),
        constraints: vec!["quote only figures on the page".into()],
        result_summary: "answered with figures quoted from the page".into(),
        finished: true,
        verification: Some(Verification::new(true, VerificationMethod::PageEvidence, "figures found on the page")),
        ..Default::default()
    }
}

fn lenient(s: &LearningService) {
    let mut st = s.settings();
    st.similarity_threshold = 0.3;
    s.update_settings(&st).unwrap();
}

/// Run every due job (jobs that failed are due again far in the future).
fn drain(s: &LearningService) {
    let mut now = now_secs();
    for _ in 0..12 {
        if !s.process_one(now).unwrap() {
            now += 3600;
            if !s.process_one(now).unwrap() {
                break;
            }
        }
    }
}

fn retrieve(s: &Arc<LearningService>, goal: &str) -> RetrievalResult {
    s.retrieve_for(&analyze_task(goal, &tools(), ""))
}

#[test]
fn the_whole_loop_learns_retrieves_and_credits_reuse() {
    let s = svc("loop");
    lenient(&s);
    // 1. A task is solved and verified; learning is queued and runs in the background.
    let job = s.enqueue_learning(&record("tell me today stock market condition")).expect("queued");
    assert!(job > 0);
    drain(&s);
    let st = s.stats().unwrap();
    assert_eq!((st.experiences_created, st.total_experiences), (1, 1));
    // 2. A similar task retrieves it.
    let r = retrieve(&s, "what is the stock market condition today");
    assert_eq!(r.selected.len(), 1, "rejected: {:?}", r.rejected);
    let used = UsedExperience { experience_id: r.selected[0].experience.id, usage_id: r.selected[0].usage_id };
    // 3. That task is solved and verified with the experience in use: reuse is credited.
    let mut second = record("what is the stock market condition today");
    second.task_id = "t2".into();
    second.used_experiences = vec![used];
    s.enqueue_learning(&second).unwrap();
    drain(&s);
    let e = s.list(&ListFilter::default()).unwrap().remove(0);
    assert_eq!((e.reuse_count, e.successful_reuse_count, e.failed_reuse_count), (1, 1, 0));
    assert_eq!(s.list(&ListFilter::default()).unwrap().len(), 1, "the similar solution updated the experience, it did not add another");
    let st = s.stats().unwrap();
    assert_eq!((st.successful_reuses, st.retrievals, st.retrieval_hits), (1, 1, 1));
    assert!(st.experiences_updated >= 1 && st.duplicates_prevented >= 1);
}

#[test]
fn switching_learning_off_stops_learning_but_not_the_agent() {
    let s = svc("off");
    lenient(&s);
    s.enqueue_learning(&record("tell me today stock market condition")).unwrap();
    drain(&s);
    assert_eq!(s.stats().unwrap().total_experiences, 1);

    let mut st = s.settings();
    st.learning_enabled = false;
    s.update_settings(&st).unwrap();
    // Nothing is queued while off...
    assert!(s.enqueue_learning(&record("read the bitcoin price from a page today")).is_none());
    // ...a job that was already waiting does not run...
    let mut on = s.settings();
    on.learning_enabled = true;
    s.update_settings(&on).unwrap();
    let waiting = s.enqueue_learning(&record("read the weather forecast for pune today")).unwrap();
    st.learning_enabled = false;
    s.update_settings(&st).unwrap();
    assert!(!s.process_one(now_secs()).unwrap(), "the master switch stops the worker");
    assert_eq!(s.recent_jobs(10).unwrap().iter().find(|j| j.id == waiting).unwrap().status, "queued");
    // ...and existing memory can still be used by the agent (retrieval is its own switch).
    assert_eq!(retrieve(&s, "tell me today stock market condition").selected.len(), 1);
    // Turning it back on lets the waiting job finish.
    on.learning_enabled = true;
    s.update_settings(&on).unwrap();
    drain(&s);
    assert_eq!(s.recent_jobs(10).unwrap().iter().find(|j| j.id == waiting).unwrap().status, "done");
}

#[test]
fn retrieval_and_automatic_creation_are_independent_switches() {
    let s = svc("switches");
    lenient(&s);
    s.enqueue_learning(&record("tell me today stock market condition")).unwrap();
    drain(&s);
    // Retrieval off: nothing is offered, learning continues.
    let mut st = s.settings();
    st.retrieval_enabled = false;
    s.update_settings(&st).unwrap();
    assert!(retrieve(&s, "tell me today stock market condition").selected.is_empty());
    // Automatic creation off: a new task creates nothing, but reuse feedback is still recorded.
    st.retrieval_enabled = true;
    st.auto_memory_creation = false;
    s.update_settings(&st).unwrap();
    let r = retrieve(&s, "tell me today stock market condition");
    let mut t = record("read the weather forecast for pune today and quote it");
    t.used_experiences = vec![UsedExperience { experience_id: r.selected[0].experience.id, usage_id: r.selected[0].usage_id }];
    s.enqueue_learning(&t).unwrap();
    drain(&s);
    let stats = s.stats().unwrap();
    assert_eq!(stats.total_experiences, 1, "no new experience while automatic creation is off");
    assert_eq!(stats.successful_reuses, 1, "the outcome of using an existing one is still recorded");
}

#[test]
fn a_broken_embedding_service_never_breaks_the_agent_and_jobs_end_as_dead_letters() {
    struct Down;
    impl EmbeddingProvider for Down {
        fn model(&self) -> &str { "down" }
        fn version(&self) -> u32 { 1 }
        fn dim(&self) -> usize { 384 }
        fn embed(&self, _: &str) -> Result<Vec<f32>, String> { Err("connection refused".into()) }
    }
    let s = LearningService::manual(&root("down"), Arc::new(Down));
    // The agent's retrieval returns an empty result with the error, it does not fail.
    let r = retrieve(&s, "tell me today stock market condition");
    assert!(r.selected.is_empty() && r.memory_error.as_deref() == Some("connection refused"));
    // Learning is queued, retried with backoff, then parked as dead: never raised to the caller.
    let id = s.enqueue_learning(&record("tell me today stock market condition")).unwrap();
    drain(&s);
    let job = s.recent_jobs(10).unwrap().into_iter().find(|j| j.id == id).unwrap();
    assert_eq!((job.status.as_str(), job.attempts), ("dead", 3));
    assert!(job.last_error.contains("connection refused"));
    let status = s.status();
    assert!(!status.embedding.ok && status.embedding.error.is_some());
    assert_eq!(status.jobs_dead, 1);
}

#[test]
fn a_panicking_worker_step_is_contained() {
    struct Panics;
    impl EmbeddingProvider for Panics {
        fn model(&self) -> &str { "panics" }
        fn version(&self) -> u32 { 1 }
        fn dim(&self) -> usize { 384 }
        fn embed(&self, _: &str) -> Result<Vec<f32>, String> { panic!("boom") }
    }
    let s = LearningService::manual(&root("panic"), Arc::new(Panics));
    let r = retrieve(&s, "tell me today stock market condition");
    assert_eq!(r.memory_error.as_deref(), Some("memory search panicked"));
    let id = s.enqueue_learning(&record("tell me today stock market condition")).unwrap();
    assert!(s.process_one(now_secs()).unwrap(), "the worker survives the panic");
    let job = s.recent_jobs(5).unwrap().into_iter().find(|j| j.id == id).unwrap();
    assert_eq!(job.status, "queued");
    assert!(job.last_error.contains("panicked"));
}

#[test]
fn the_queue_is_bounded() {
    let s = svc("queue");
    let mut st = s.settings();
    st.max_background_jobs = 1;
    s.update_settings(&st).unwrap();
    let mut accepted = 0;
    for i in 0..30 {
        if s.enqueue_learning(&record(&format!("tell me today stock market condition number {i}"))).is_some() {
            accepted += 1;
        }
    }
    assert_eq!(accepted, 10, "one worker allows ten waiting jobs");
    assert!(s.store().recent_events(50).unwrap().iter().any(|e| e.event == "learning_job_dropped"));
}

#[test]
fn people_can_approve_trust_reject_and_deprecate() {
    let s = svc("manage");
    s.enqueue_learning(&record("tell me today stock market condition")).unwrap();
    drain(&s);
    let id = s.list(&ListFilter::default()).unwrap()[0].id;
    assert_eq!(s.set_status(id, "approve").unwrap().status, ExperienceStatus::Trusted);
    assert_eq!(s.set_status(id, "deprecate").unwrap().status, ExperienceStatus::Deprecated);
    assert!(!s.store().get(id).unwrap().unwrap().human_approved, "deprecating withdraws the approval");
    let back = s.revalidate(id, &tools()).unwrap();
    assert_eq!(back.status, "VERIFIED", "a withdrawn approval is not silently restored by revalidation");
    s.set_status(id, "deprecate").unwrap();
    assert_eq!(s.set_status(id, "reject").unwrap().status, ExperienceStatus::Invalid);
    assert!(s.set_status(id, "explode").is_err());
    assert!(s.set_status(9999, "approve").is_err());
    // An unverified experience cannot be promoted by hand.
    let mut unverified = Experience::candidate("an unverified idea about markets", "live_data");
    unverified.embedding_model = "aqua-hash-384".into();
    unverified.embedding_version = 1;
    let uid = s.store().insert(&unverified, &HashEmbedding.embed(&embedding_text(&unverified)).unwrap()).unwrap();
    assert!(s.set_status(uid, "trust").unwrap_err().contains("unverified"));
    assert!(s.delete(id).unwrap() && !s.delete(id).unwrap());
    assert_eq!(s.clear_all().unwrap(), 1);
    assert!(s.store().recent_events(50).unwrap().iter().any(|e| e.event == "experience_status_changed"));
}

#[test]
fn revalidation_restores_a_still_valid_experience_and_deprecates_one_that_no_longer_fits() {
    let s = svc("reval");
    s.enqueue_learning(&record("tell me today stock market condition")).unwrap();
    drain(&s);
    let id = s.list(&ListFilter::default()).unwrap()[0].id;
    // Make it stale.
    let mut e = s.store().get(id).unwrap().unwrap();
    e.status = ExperienceStatus::Stale;
    e.last_verified_at = now_secs() - 200 * 86_400;
    s.store().update(&e, None).unwrap();
    let ok = s.revalidate(id, &tools()).unwrap();
    assert!(ok.valid && ok.status == "VERIFIED", "{ok:?}");
    // The tool it needs is gone: it no longer fits.
    let gone = s.revalidate(id, &["Click".to_string()]).unwrap();
    assert!(!gone.valid && gone.status == "DEPRECATED" && gone.reason.contains("not available"), "{gone:?}");
    // A person's rejection is not overridden.
    s.set_status(id, "reject").unwrap();
    assert!(!s.revalidate(id, &tools()).unwrap().valid);
    assert_eq!(s.store().get(id).unwrap().unwrap().status, ExperienceStatus::Invalid);
}

#[test]
fn consolidation_merges_near_duplicates_keeping_their_history_and_drops_dead_weight() {
    let s = svc("consolidate");
    let add = |problem: &str, quality: f32, reuse: u32| {
        let mut e = Experience::candidate(problem, "live_data");
        e.tools_used = tools();
        e.status = ExperienceStatus::Verified;
        e.verified = true;
        e.quality_score = quality;
        e.reuse_count = reuse;
        e.successful_reuse_count = reuse;
        e.last_verified_at = now_secs();
        e.embedding_model = "aqua-hash-384".into();
        e.embedding_version = 1;
        s.store().insert(&e, &HashEmbedding.embed(&embedding_text(&e)).unwrap()).unwrap()
    };
    let best = add("read the stock market condition from a google search page", 0.9, 3);
    let dup = add("read the stock market condition from a google search page today", 0.6, 2);
    let other = add("open calculator and multiply two numbers", 0.7, 0);
    // A candidate that never got verified within a month.
    let mut stale_candidate = Experience::candidate("some unproven idea about email drafts", "email");
    stale_candidate.created_at = now_secs() - 60 * 86_400;
    stale_candidate.embedding_model = "aqua-hash-384".into();
    stale_candidate.embedding_version = 1;
    let cand = s.store().insert(&stale_candidate, &HashEmbedding.embed(&embedding_text(&stale_candidate)).unwrap()).unwrap();

    let report = s.consolidate().unwrap();
    assert_eq!((report.merged, report.removed), (1, 1), "{report:?}");
    assert!(s.store().get(dup).unwrap().is_none() && s.store().get(cand).unwrap().is_none());
    let kept = s.store().get(best).unwrap().unwrap();
    assert_eq!((kept.reuse_count, kept.successful_reuse_count), (5, 5), "the merged experience keeps both histories");
    assert!(s.store().get(other).unwrap().is_some(), "unrelated experiences are untouched");
}

#[test]
fn the_dashboard_status_reports_switches_health_and_activity() {
    let s = svc("status");
    lenient(&s);
    s.enqueue_learning(&record("tell me today stock market condition")).unwrap();
    let before = s.status();
    assert!(before.learning_enabled && before.retrieval_enabled && before.auto_memory_creation && !before.autonomous_experimentation);
    assert_eq!(before.jobs_queued, 1);
    assert!(before.embedding.ok && before.embedding.dimensions == 384 && before.vector_store.ok);
    drain(&s);
    let after = s.status();
    assert_eq!((after.jobs_queued, after.vector_store.indexed, after.vector_store.needs_reindex), (0, 1, 0));
    assert!(after.last_learning_at > 0 && after.last_success_at > 0 && after.current_activity.is_empty());
    let events: Vec<String> = s.store().recent_events(50).unwrap().into_iter().map(|e| e.event).collect();
    for expected in ["learning_job_started", "experience_created", "learning_job_completed", "settings_changed"] {
        assert!(events.iter().any(|e| e == expected), "missing event {expected}: {events:?}");
    }
}

#[test]
fn settings_are_persisted_clamped_and_survive_a_restart() {
    let dir = root("persist");
    let s = LearningService::manual(&dir, Arc::new(HashEmbedding));
    let mut st = s.settings();
    st.learning_enabled = false;
    st.max_attempts = 99;
    st.max_execution_secs = 5;
    let saved = s.update_settings(&st).unwrap();
    assert_eq!((saved.max_attempts, saved.max_execution_secs), (10, 10));
    let reopened = LearningService::open(&dir);
    assert!(!reopened.settings().learning_enabled && reopened.settings().max_attempts == 10);
}

#[test]
fn search_and_reindex_work_through_the_service() {
    let s = svc("search");
    s.enqueue_learning(&record("tell me today stock market condition")).unwrap();
    drain(&s);
    let hits = s.search("what is the stock market condition today", 3).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].1 > 0.3);
    let rep = s.reindex_all().unwrap();
    assert_eq!((rep.total, rep.reindexed, rep.failed), (1, 1, 0));
}

#[test]
fn the_real_background_worker_picks_up_jobs_on_its_own() {
    // The one test that uses the real thread: enqueue, then wait for the worker to finish the job.
    let s = LearningService::open(&root("worker"));
    let mut st = s.settings();
    st.similarity_threshold = 0.3;
    s.update_settings(&st).unwrap();
    let id = s.enqueue_learning(&record("tell me today stock market condition")).unwrap();
    crate::test_support::wait_until(std::time::Duration::from_secs(20), "the worker to finish the job", || s.recent_jobs(5).unwrap().into_iter().find(|j| j.id == id).is_some_and(|j| j.status == "done"));
    assert_eq!(s.stats().unwrap().total_experiences, 1);
    assert!(s.status().worker_running);
}

#[test]
fn the_daily_tidy_up_runs_when_due_survives_a_restart_and_waits_for_the_user() {
    let dir = root("consolidate_due");
    let s = LearningService::manual(&dir, Arc::new(HashEmbedding));
    let day = 86_400;
    // Never run: due now, unless the user is in the middle of a run.
    let guard = s.activity().begin();
    assert!(s.consolidate_if_due(1_000_000).unwrap().is_none(), "not while a user run is active");
    drop(guard);
    assert!(s.consolidate_if_due(1_000_000).unwrap().is_some(), "due when it has never run");
    // Not due again until a day has passed.
    assert!(s.consolidate_if_due(1_000_000 + day - 1).unwrap().is_none());
    // The schedule is in the database: a new service on the same folder does not run it early.
    let again = LearningService::manual(&dir, Arc::new(HashEmbedding));
    assert!(again.consolidate_if_due(1_000_000 + 3600).unwrap().is_none());
    assert!(again.consolidate_if_due(1_000_000 + day).unwrap().is_some());
    assert!(again.store().recent_events(20).unwrap().iter().any(|e| e.event == "memory_consolidated"));
}

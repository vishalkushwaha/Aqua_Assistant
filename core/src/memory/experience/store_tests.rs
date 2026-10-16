//! Tests for the experience store and the vector layer.

use super::model::*;
use super::settings::LearningSettings;
use super::store::*;
use super::vector::{cosine, embedding_text, reindex, BruteForceIndex, EmbeddingProvider, HashEmbedding, VectorIndex};

fn store(name: &str) -> ExperienceStore {
    let d = std::env::temp_dir().join(format!("aqua_exp_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    ExperienceStore::new(&d)
}

fn exp(problem: &str, scope: ExperienceScope, project: &str, owner: &str) -> Experience {
    let mut e = Experience::candidate(problem, "live_data");
    e.scope = scope;
    e.project_key = project.into();
    e.owner_id = owner.into();
    e.approach = "search then read the snapshot".into();
    e.embedding_model = HashEmbedding.model().into();
    e.embedding_version = HashEmbedding.version();
    e
}

fn add(s: &ExperienceStore, e: &Experience) -> i64 {
    s.insert(e, &HashEmbedding.embed(&embedding_text(e)).unwrap()).unwrap()
}

#[test]
fn an_experience_survives_a_round_trip_and_update() {
    let s = store("roundtrip");
    let mut e = exp("read the stock market page", ExperienceScope::Global, "", "");
    e.important_steps = vec!["navigate".into(), "snapshot".into()];
    e.tools_used = vec!["browser_navigate".into()];
    e.failed_approach = Some("guess from memory".into());
    let id = add(&s, &e);
    let mut got = s.get(id).unwrap().unwrap();
    assert_eq!(got.important_steps, vec!["navigate", "snapshot"]);
    assert_eq!(got.failed_approach.as_deref(), Some("guess from memory"));
    got.confidence = 0.8;
    got.status = ExperienceStatus::Verified;
    s.update(&got, None).unwrap();
    let again = s.get(id).unwrap().unwrap();
    assert_eq!((again.confidence, again.status), (0.8, ExperienceStatus::Verified));
    assert!(s.delete(id).unwrap() && s.get(id).unwrap().is_none() && !s.delete(id).unwrap());
}

#[test]
fn scopes_do_not_leak_between_projects_or_users() {
    let s = store("scopes");
    add(&s, &exp("global tip", ExperienceScope::Global, "", ""));
    add(&s, &exp("google only", ExperienceScope::Project, "web:google.com", ""));
    add(&s, &exp("other site only", ExperienceScope::Project, "web:other.test", ""));
    add(&s, &exp("alice habit", ExperienceScope::User, "", "alice"));
    add(&s, &exp("bob habit", ExperienceScope::User, "", "bob"));
    let seen = |project: &str, owner: &str| -> Vec<String> {
        let mut v: Vec<String> = s
            .visible_with_embeddings(&Viewer { project_key: project.into(), owner_id: owner.into() })
            .unwrap()
            .into_iter()
            .map(|(e, _)| e.problem_summary)
            .collect();
        v.sort();
        v
    };
    assert_eq!(seen("web:google.com", "alice"), vec!["alice habit", "global tip", "google only"]);
    assert_eq!(seen("web:other.test", "bob"), vec!["bob habit", "global tip", "other site only"]);
    // Nobody identified: only what is global. An empty project or owner never matches an empty field.
    assert_eq!(seen("", ""), vec!["global tip"]);
    assert_eq!(seen("web:unknown", "carol"), vec!["global tip"]);
}

#[test]
fn list_filters_and_searches() {
    let s = store("list");
    let mut a = exp("market page reading", ExperienceScope::Global, "", "");
    a.status = ExperienceStatus::Trusted;
    add(&s, &a);
    let mut b = exp("weather lookup", ExperienceScope::Global, "", "");
    b.experience_type = ExperienceType::ErrorFix;
    add(&s, &b);
    assert_eq!(s.list(&ListFilter::default()).unwrap().len(), 2);
    assert_eq!(s.list(&ListFilter { status: Some(ExperienceStatus::Trusted), ..Default::default() }).unwrap().len(), 1);
    assert_eq!(s.list(&ListFilter { experience_type: Some(ExperienceType::ErrorFix), ..Default::default() }).unwrap().len(), 1);
    assert_eq!(s.list(&ListFilter { text: Some("market".into()), ..Default::default() }).unwrap().len(), 1);
    // A lone percent sign is not a wildcard: it is stripped, leaving an empty search that matches all.
    assert_eq!(s.list(&ListFilter { text: Some("%".into()), ..Default::default() }).unwrap().len(), 2);
    assert_eq!(s.list(&ListFilter { text: Some("zzz%".into()), ..Default::default() }).unwrap().len(), 0);
}

#[test]
fn settings_persist_and_are_clamped() {
    let s = store("settings");
    assert_eq!(s.load_settings(), LearningSettings::default());
    let mut changed = LearningSettings::default();
    changed.learning_enabled = false;
    changed.max_attempts = 0;
    let saved = s.save_settings(&changed).unwrap();
    assert_eq!(saved.max_attempts, 1);
    // A new handle on the same file (a restart) sees the saved values.
    let reopened = ExperienceStore::new(s.path().parent().unwrap().parent().unwrap());
    let loaded = reopened.load_settings();
    assert!(!loaded.learning_enabled && loaded.max_attempts == 1);
}

#[test]
fn failed_jobs_back_off_then_become_dead_letters() {
    let s = store("jobs");
    let id = s.enqueue_job("learn", &serde_json::json!({"task": "x"}), 3).unwrap();
    let now = now_secs();
    let job = s.claim_next_job(now).unwrap().unwrap();
    assert_eq!((job.id, job.status.as_str()), (id, "running"));
    assert!(s.claim_next_job(now).unwrap().is_none(), "a running job is not handed out twice");
    assert_eq!(s.fail_job(id, "embedding service down", now).unwrap(), "queued");
    assert!(s.claim_next_job(now).unwrap().is_none(), "backoff: not due yet");
    let due = now + 60;
    s.claim_next_job(due).unwrap().unwrap();
    assert_eq!(s.fail_job(id, "still down", due).unwrap(), "queued");
    s.claim_next_job(due + 3600).unwrap().unwrap();
    assert_eq!(s.fail_job(id, "gave up", due + 3600).unwrap(), "dead");
    assert!(s.claim_next_job(due + 100_000).unwrap().is_none(), "dead jobs are never retried");
    let dead = s.list_jobs(Some("dead"), 10).unwrap();
    assert_eq!((dead.len(), dead[0].attempts), (1, 3));
    assert!(dead[0].last_error.contains("gave up"));
}

#[test]
fn a_crash_leaves_no_job_stuck_running() {
    let s = store("crash");
    s.enqueue_job("learn", &serde_json::json!({}), 3).unwrap();
    s.claim_next_job(now_secs()).unwrap().unwrap();
    assert_eq!(s.requeue_interrupted_jobs().unwrap(), 1);
    assert!(s.claim_next_job(now_secs()).unwrap().is_some());
}

#[test]
fn stats_count_what_happened() {
    let s = store("stats");
    let mut e = exp("x", ExperienceScope::Global, "", "");
    e.quality_score = 0.8;
    e.reuse_count = 3;
    e.successful_reuse_count = 2;
    e.failed_reuse_count = 1;
    e.status = ExperienceStatus::Deprecated;
    add(&s, &e);
    s.bump("tasks_analyzed", 4.0).unwrap();
    s.bump("retrievals", 4.0).unwrap();
    s.bump("retrieval_hits", 1.0).unwrap();
    s.bump("similarity_sum", 1.5).unwrap();
    s.bump("similarity_n", 3.0).unwrap();
    let st = s.stats().unwrap();
    assert_eq!((st.total_experiences, st.experiences_reused, st.successful_reuses, st.failed_reuses), (1, 3, 2, 1));
    assert_eq!((st.tasks_analyzed, st.retrievals, st.experiences_deprecated), (4, 4, 1));
    assert!((st.retrieval_hit_rate - 0.25).abs() < 1e-6);
    assert!((st.average_similarity - 0.5).abs() < 1e-6);
    assert!((st.average_quality - 0.8).abs() < 1e-6);
}

#[test]
fn events_are_recorded_newest_first() {
    let s = store("events");
    s.log_event("task_received", &serde_json::json!({"n": 1}), None);
    s.log_event("learning_job_started", &serde_json::json!({}), Some(7));
    let ev = s.recent_events(10).unwrap();
    assert_eq!((ev[0].event.as_str(), ev[0].job_id, ev[1].event.as_str()), ("learning_job_started", Some(7), "task_received"));
}

#[test]
fn the_index_finds_similar_tasks_and_never_mixes_embedding_models() {
    let s = store("index");
    let a = exp("read the stock market condition from a google page", ExperienceScope::Global, "", "");
    let b = exp("open calculator and multiply two numbers", ExperienceScope::Global, "", "");
    let mut old = exp("stock market from an old embedding model", ExperienceScope::Global, "", "");
    old.embedding_model = "some-other-model".into();
    let ida = add(&s, &a);
    add(&s, &b);
    add(&s, &old);
    let rows = s.visible_with_embeddings(&Viewer::default()).unwrap();
    let index = BruteForceIndex::build(&rows, &HashEmbedding);
    assert_eq!((index.len(), index.skipped_incompatible), (2, 1));
    let q = HashEmbedding.embed("tell me the stock market condition from google").unwrap();
    let hits = index.search(&q, 5);
    assert_eq!(hits[0].0, ida, "the related experience ranks first: {hits:?}");
    assert!(hits[0].1 > hits[1].1);
    assert_eq!(cosine(&[1.0, 0.0], &[1.0]), 0.0, "different sizes never compare");
}

struct V2;
impl EmbeddingProvider for V2 {
    fn model(&self) -> &str {
        "aqua-hash-384"
    }
    fn version(&self) -> u32 {
        2
    }
    fn dim(&self) -> usize {
        384
    }
    fn embed(&self, t: &str) -> Result<Vec<f32>, String> {
        HashEmbedding.embed(t)
    }
}

struct Broken;
impl EmbeddingProvider for Broken {
    fn model(&self) -> &str {
        "broken"
    }
    fn version(&self) -> u32 {
        1
    }
    fn dim(&self) -> usize {
        384
    }
    fn embed(&self, _: &str) -> Result<Vec<f32>, String> {
        Err("service down".into())
    }
}

#[test]
fn reindexing_switches_every_vector_to_the_new_model_or_changes_nothing() {
    let s = store("reindex");
    add(&s, &exp("first", ExperienceScope::Global, "", ""));
    add(&s, &exp("second", ExperienceScope::Global, "", ""));
    let failed = reindex(&s, &Broken).unwrap();
    assert_eq!((failed.reindexed, failed.failed), (0, 2));
    assert!(
        s.list(&ListFilter::default()).unwrap().iter().all(|e| e.embedding_model == "aqua-hash-384" && e.embedding_version == 1),
        "a failed reindex writes nothing"
    );
    let done = reindex(&s, &V2).unwrap();
    assert_eq!((done.total, done.reindexed, done.failed), (2, 2, 0));
    assert!(s.list(&ListFilter::default()).unwrap().iter().all(|e| e.embedding_version == 2));
    let rows = s.visible_with_embeddings(&Viewer::default()).unwrap();
    assert_eq!(BruteForceIndex::build(&rows, &V2).len(), 2);
    assert_eq!(BruteForceIndex::build(&rows, &HashEmbedding).len(), 0, "old-version queries no longer match new vectors");
}

#[test]
fn history_tables_are_pruned_by_age_and_every_connection_overwrites_deleted_text() {
    let s = store("prune_history");
    let c = s.conn().unwrap();
    let secure: i64 = c.query_row("PRAGMA secure_delete", [], |r| r.get(0)).unwrap();
    assert_eq!(secure, 1, "each connection sets secure_delete");
    let old = now_secs() - 200 * 86_400;
    let fresh = now_secs() - 86_400;
    c.execute("INSERT INTO agent_experience_usage (experience_id, used_at, task_summary, similarity) VALUES (1, ?1, 'old', 0.9), (1, ?2, 'new', 0.9)", rusqlite::params![old, fresh]).unwrap();
    for i in 0..205 {
        c.execute("INSERT INTO agent_practice_tasks (goal, family, source, status, created_at, started_at) VALUES (?1, 'f', 's', 'done', ?2, ?2)", rusqlite::params![format!("g{i}"), old]).unwrap();
    }
    c.execute("INSERT INTO agent_learning_jobs (kind, payload, status, created_at, updated_at) VALUES ('learn', '{}', 'dead', ?1, ?1), ('learn', '{}', 'dead', ?2, ?2)", rusqlite::params![old, fresh]).unwrap();
    drop(c);
    let removed = s.prune_history().unwrap();
    assert_eq!(removed, 1 + 5 + 1, "one old usage row, five practice tasks beyond the newest 200, one old dead job");
    let c = s.conn().unwrap();
    let usage: i64 = c.query_row("SELECT COUNT(*) FROM agent_experience_usage", [], |r| r.get(0)).unwrap();
    let practice: i64 = c.query_row("SELECT COUNT(*) FROM agent_practice_tasks", [], |r| r.get(0)).unwrap();
    let dead: i64 = c.query_row("SELECT COUNT(*) FROM agent_learning_jobs WHERE status='dead'", [], |r| r.get(0)).unwrap();
    assert_eq!((usage, practice, dead), (1, 200, 1), "the recent usage row, the newest 200 practice tasks and the recent dead job stay");
}

// ---- the file (OA3-02: the shared SQLite helper) ------------------------------------------------------------

fn base_of(s: &ExperienceStore) -> std::path::PathBuf {
    s.path().parent().unwrap().parent().unwrap().to_path_buf()
}

fn version_of_file(s: &ExperienceStore) -> i64 {
    rusqlite::Connection::open(s.path()).unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
}

#[test]
fn the_experience_file_has_a_version_and_a_file_made_before_versions_keeps_its_rows_when_it_is_stamped() {
    let s = store("versions");
    assert_eq!(version_of_file(&s), 1, "a new file is at the first step");
    let id = add(&s, &exp("read the stock market page", ExperienceScope::Global, "", ""));
    // The file as the build before this one left it: the tables, the rows, and no version.
    {
        let c = rusqlite::Connection::open(s.path()).unwrap();
        c.execute_batch("PRAGMA user_version = 0;").unwrap();
    }
    let again = ExperienceStore::new(base_of(&s));
    assert_eq!(version_of_file(&again), 1, "stamped");
    assert_eq!(again.count().unwrap(), 1);
    assert!(again.get(id).unwrap().is_some(), "the row is where it was");
    // Deleted text is still overwritten on every connection.
    let on: i64 = again.conn().unwrap().query_row("PRAGMA secure_delete", [], |r| r.get(0)).unwrap();
    assert_eq!(on, 1);
}

#[test]
fn a_damaged_experience_file_is_moved_aside_and_the_store_works_in_a_new_one() {
    let d = std::env::temp_dir().join(format!("aqua_exp_damaged_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("data")).unwrap();
    std::fs::write(d.join("data").join("experience.db"), "this is not a database, only text. ".repeat(300)).unwrap();
    let s = ExperienceStore::new(&d);
    assert_eq!(s.count().unwrap(), 0, "a new, working file instead of an error on every call");
    let id = add(&s, &exp("after the recovery", ExperienceScope::Global, "", ""));
    assert!(s.get(id).unwrap().is_some());
    let aside = crate::storage::sqlite_store::leftovers(&d);
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert!(std::fs::read_to_string(&aside[0]).unwrap().starts_with("this is not a database"), "the old file is kept, not deleted");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_experience_file_from_a_newer_build_is_refused_call_by_call_and_left_alone() {
    let s = store("newer");
    add(&s, &exp("kept", ExperienceScope::Global, "", ""));
    {
        let c = rusqlite::Connection::open(s.path()).unwrap();
        let _: String = c.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0)).unwrap();
        c.execute_batch("PRAGMA user_version = 42;").unwrap();
    }
    let before = std::fs::read(s.path()).unwrap();
    let newer = ExperienceStore::new(base_of(&s));
    let err = newer.count().unwrap_err();
    assert!(err.contains("newer than this build") && err.contains("version 42"), "{err}");
    assert!(newer.insert(&exp("must not be written", ExperienceScope::Global, "", ""), &[0.0; 4]).is_err());
    assert_eq!(std::fs::read(s.path()).unwrap(), before, "byte for byte what it was");
}

#[test]
fn four_threads_write_and_read_the_store_at_once() {
    let s = store("threads");
    let handles: Vec<_> = (0..4)
        .map(|t| {
            let s = s.clone();
            std::thread::spawn(move || {
                for i in 0..15 {
                    add(&s, &exp(&format!("thread {t} experience {i}"), ExperienceScope::Global, "", ""));
                    assert!(s.count().unwrap() >= 1);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(s.count().unwrap(), 60);
}

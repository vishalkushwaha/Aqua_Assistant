//! Tests of the shared SQLite helper (card OA3-02). Every test makes its own throw-away folder (`TempWorld`) and never touches `data/`.

use super::*;
use crate::test_support::TempWorld;
use rusqlite::Connection;

const STEP1: Migration = Migration { id: 1, sql: "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL); INSERT INTO notes (body) VALUES ('first');" };
const STEP2: Migration = Migration { id: 2, sql: "ALTER TABLE notes ADD COLUMN tag TEXT NOT NULL DEFAULT '';" };
const STEP3: Migration = Migration { id: 3, sql: "CREATE TABLE extra (n INTEGER); INSERT INTO extra VALUES (3);" };

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap()
}

fn has_table(conn: &Connection, name: &str) -> bool {
    conn.query_row("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1", [name], |r| r.get::<_, i64>(0)).unwrap() > 0
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

#[test]
fn a_fresh_database_is_made_with_every_step_and_the_version_of_the_last() {
    let w = TempWorld::new("sqlite_fresh");
    let path = w.path("data/store.db");
    let opened = open_migrated(&path, &Opts::standard(), &[STEP1, STEP2, STEP3]).unwrap();
    assert!(opened.recovered_from.is_none());
    assert_eq!(version(&opened.conn).unwrap(), 3);
    assert_eq!(count(&opened.conn, "notes"), 1);
    assert_eq!(count(&opened.conn, "extra"), 1);
    let mode: String = opened.conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
    // A database made from nothing has nothing to back up.
    assert!(!names(&w.path("data")).iter().any(|n| n.contains(".bak-v")), "{:?}", names(&w.path("data")));
}

#[test]
fn opening_again_is_idempotent_and_keeps_the_rows() {
    let w = TempWorld::new("sqlite_idem");
    let path = w.path("store.db");
    {
        let o = open_migrated(&path, &Opts::standard(), &[STEP1, STEP2]).unwrap();
        o.conn.execute("INSERT INTO notes (body, tag) VALUES ('second', 't')", []).unwrap();
    }
    let o = open_migrated(&path, &Opts::standard(), &[STEP1, STEP2]).unwrap();
    assert_eq!(version(&o.conn).unwrap(), 2);
    assert_eq!(count(&o.conn, "notes"), 2, "the first step did not run again (it would have inserted 'first' a second time)");
    assert!(!names(&w.root).iter().any(|n| n.contains(".bak-v") || n.contains(".corrupt-")), "nothing to back up and nothing to move aside: {:?}", names(&w.root));
}

#[test]
fn a_failing_second_statement_leaves_the_version_and_the_schema_as_they_were() {
    let w = TempWorld::new("sqlite_rollback");
    let path = w.path("store.db");
    drop(open_migrated(&path, &Opts::standard(), &[STEP1]).unwrap());
    // The step adds a table and then fails on its second statement: neither the table nor the new version may stay.
    let bad = Migration { id: 2, sql: "CREATE TABLE half_done (x INTEGER); INSERT INTO missing_table VALUES (1);" };
    let err = open_migrated(&path, &Opts::standard(), &[STEP1, bad]).unwrap_err();
    assert!(matches!(err, StoreError::Step { id: 2, .. }), "{err}");
    assert!(err.to_string().contains("step 2") && err.to_string().contains("rolled back"), "{err}");
    let conn = Connection::open(&path).unwrap();
    assert_eq!(version(&conn).unwrap(), 1, "the version was not raised");
    assert!(!has_table(&conn, "half_done"), "the first statement of the step was rolled back");
    // And a step with an ALTER that fails half-way (the case that used to leave a column added and the old version): same.
    let alter_then_fail = Migration { id: 2, sql: "ALTER TABLE notes ADD COLUMN tag TEXT NOT NULL DEFAULT ''; ALTER TABLE notes ADD COLUMN body TEXT;" };
    assert!(open_migrated(&path, &Opts::standard(), &[STEP1, alter_then_fail]).is_err());
    let conn = Connection::open(&path).unwrap();
    assert_eq!(version(&conn).unwrap(), 1);
    let columns: Vec<String> = conn.prepare("PRAGMA table_info(notes)").unwrap().query_map([], |r| r.get::<_, String>(1)).unwrap().map(Result::unwrap).collect();
    assert_eq!(columns, vec!["id".to_string(), "body".to_string()], "the added column did not stay");
    // The corrected step then runs from where it stopped.
    let o = open_migrated(&path, &Opts::standard(), &[STEP1, STEP2]).unwrap();
    assert_eq!(version(&o.conn).unwrap(), 2);
}

#[test]
fn a_first_step_that_fails_leaves_a_database_with_no_version_and_no_table() {
    let w = TempWorld::new("sqlite_first_fails");
    let path = w.path("store.db");
    let bad = Migration { id: 1, sql: "CREATE TABLE a (x INTEGER); NOT SQL AT ALL;" };
    assert!(open_migrated(&path, &Opts::standard(), &[bad]).is_err());
    let conn = Connection::open(&path).unwrap();
    assert_eq!(version(&conn).unwrap(), 0);
    assert!(!has_table(&conn, "a"));
}

#[test]
fn a_garbage_file_is_moved_aside_and_a_new_database_takes_its_place() {
    let w = TempWorld::new("sqlite_garbage");
    let path = w.write("data/store.db", "this is not a database, it is a few lines of text\n".repeat(200));
    let garbage = std::fs::read(&path).unwrap();
    let o = open_migrated(&path, &Opts::standard(), &[STEP1, STEP2]).unwrap();
    let aside = o.recovered_from.clone().expect("the damaged file was moved aside");
    assert_eq!(std::fs::read(&aside).unwrap(), garbage, "the old file is kept, byte for byte");
    let name = aside.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("store.db.corrupt-"), "{name}");
    assert_eq!(version(&o.conn).unwrap(), 2, "the new database has the schema");
    assert_eq!(count(&o.conn, "notes"), 1);
    // The doctor can find it.
    assert_eq!(leftovers(&w.path("data")), vec![aside]);
}

#[test]
fn a_database_cut_short_is_moved_aside_and_its_stale_log_is_not_replayed_onto_the_new_one() {
    let w = TempWorld::new("sqlite_truncated");
    let path = w.path("store.db");
    {
        let o = open_migrated(&path, &Opts::standard(), &[STEP1]).unwrap();
        for i in 0..200 {
            o.conn.execute("INSERT INTO notes (body) VALUES (?1)", [format!("row {i} {}", "x".repeat(100))]).unwrap();
        }
        o.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    }
    let full = std::fs::read(&path).unwrap();
    assert!(full.len() > 4096);
    std::fs::write(&path, &full[..100]).unwrap();
    std::fs::write(w.path("store.db-wal"), b"stale log").unwrap();
    std::fs::write(w.path("store.db-shm"), b"stale index").unwrap();
    let o = open_migrated(&path, &Opts::standard(), &[STEP1]).unwrap();
    let aside = o.recovered_from.clone().expect("moved aside");
    assert_eq!(count(&o.conn, "notes"), 1, "a new database, not the damaged one");
    assert_eq!(std::fs::read(&aside).unwrap(), &full[..100]);
    assert_eq!(leftovers(&w.root), vec![aside], "only the main file of a damaged pair is listed");
}

#[test]
fn the_log_files_of_a_damaged_database_travel_with_it() {
    let w = TempWorld::new("sqlite_aside");
    let path = w.write("store.db", "main");
    w.write("store.db-wal", "log");
    w.write("store.db-shm", "index");
    let aside = move_aside(&path).unwrap();
    let base = aside.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(names(&w.root), vec![base.clone(), format!("{base}-shm"), format!("{base}-wal")]);
    assert_eq!(std::fs::read_to_string(&aside).unwrap(), "main");
    assert_eq!(std::fs::read_to_string(w.path(&format!("{base}-wal"))).unwrap(), "log");
    // A second one in the same second gets its own name.
    w.write("store.db", "main again");
    let second = move_aside(&path).unwrap();
    assert_ne!(second, aside);
    assert_eq!(std::fs::read_to_string(&second).unwrap(), "main again");
    assert_eq!(std::fs::read_to_string(&aside).unwrap(), "main", "the first one is untouched");
}

#[test]
fn a_damaged_page_in_the_middle_is_found_by_the_check() {
    let w = TempWorld::new("sqlite_page");
    let path = w.path("store.db");
    {
        let o = open_migrated(&path, &Opts::standard().journal(Journal::Delete), &[STEP1]).unwrap();
        for i in 0..400 {
            o.conn.execute("INSERT INTO notes (body) VALUES (?1)", [format!("row {i} {}", "y".repeat(200))]).unwrap();
        }
    }
    let mut bytes = std::fs::read(&path).unwrap();
    assert!(bytes.len() > 3 * 4096);
    // Overwrite a data page (not the header) with noise.
    for (i, b) in bytes[4096..8192].iter_mut().enumerate() {
        *b = (i * 31 % 251) as u8;
    }
    std::fs::write(&path, &bytes).unwrap();
    let o = open_migrated(&path, &Opts::standard(), &[STEP1]).unwrap();
    assert!(o.recovered_from.is_some(), "quick_check found the damage");
    assert_eq!(count(&o.conn, "notes"), 1);
    // With the check off the same file is opened as it is (the file index, which is too big to scan at every start, does this).
    let w2 = TempWorld::new("sqlite_page_nocheck");
    let p2 = w2.write("store.db", &bytes);
    let o2 = open(&p2, &Opts::standard().journal(Journal::Keep).quick_check(false)).unwrap();
    assert!(o2.recovered_from.is_none());
}

#[test]
fn a_store_that_must_not_reset_silently_gets_the_error_and_keeps_the_file_where_it_is() {
    let w = TempWorld::new("sqlite_no_recover");
    let path = w.write("store.db", "not a database ".repeat(400));
    let err = open_migrated(&path, &Opts::standard().recover(false), &[STEP1]).unwrap_err();
    assert!(err.is_corruption(), "{err}");
    assert!(path.is_file() && !names(&w.root).iter().any(|n| n.contains(".corrupt-")), "{:?}", names(&w.root));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "not a database ".repeat(400), "untouched");
}

#[test]
fn a_file_bigger_than_the_limit_is_not_scanned_but_a_file_that_is_not_a_database_is_still_found() {
    let w = TempWorld::new("sqlite_big");
    let path = w.path("store.db");
    {
        let o = open_migrated(&path, &Opts::standard().journal(Journal::Delete), &[STEP1]).unwrap();
        for i in 0..400 {
            o.conn.execute("INSERT INTO notes (body) VALUES (?1)", [format!("row {i} {}", "y".repeat(200))]).unwrap();
        }
    }
    let mut bytes = std::fs::read(&path).unwrap();
    for (i, b) in bytes[4096..8192].iter_mut().enumerate() {
        *b = (i * 31 % 251) as u8;
    }
    std::fs::write(&path, &bytes).unwrap();
    // Over the limit: no full scan, so the damaged page is not looked for at start (it is found when it is read).
    let small_limit = Opts::standard().journal(Journal::Keep).quick_check_up_to(1024);
    assert!(open(&path, &small_limit).unwrap().recovered_from.is_none());
    // Under the limit it is found.
    assert!(open(&path, &Opts::standard().journal(Journal::Keep)).unwrap().recovered_from.is_some());
    // A file that is not a database at all is found whatever its size.
    let junk = w.write("junk.db", "text, not pages. ".repeat(500));
    assert!(open(&junk, &small_limit).unwrap().recovered_from.is_some());
}

#[test]
fn a_locked_database_is_an_error_and_is_never_moved_aside() {
    let w = TempWorld::new("sqlite_locked");
    let path = w.path("store.db");
    {
        // The default journal mode (a rollback journal), so that an exclusive lock really keeps readers out.
        let c = Connection::open(&path).unwrap();
        c.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);").unwrap();
    }
    let holder = Connection::open(&path).unwrap();
    holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let err = open(&path, &Opts::standard().busy(150)).unwrap_err();
    assert!(!err.is_corruption(), "a lock is not damage: {err}");
    assert!(path.is_file(), "the database stays where it is");
    assert!(!names(&w.root).iter().any(|n| n.contains(".corrupt-")), "{:?}", names(&w.root));
    holder.execute_batch("ROLLBACK").unwrap();
    drop(holder);
    let o = open(&path, &Opts::standard()).unwrap();
    assert_eq!(count(&o.conn, "t"), 1);
}

#[test]
fn a_database_from_a_newer_build_is_refused_and_not_changed_by_a_single_byte() {
    let w = TempWorld::new("sqlite_too_new");
    let path = w.path("store.db");
    {
        // A newer build's file: rollback journal (the older build would switch it to a write-ahead log if it got that far), version 3.
        let o = open_migrated(&path, &Opts::standard().journal(Journal::Delete), &[STEP1, STEP2, STEP3]).unwrap();
        assert_eq!(version(&o.conn).unwrap(), 3);
    }
    let before = std::fs::read(&path).unwrap();
    let err = open_migrated(&path, &Opts::standard(), &[STEP1, STEP2]).unwrap_err();
    match &err {
        StoreError::TooNew { found, known, .. } => assert_eq!((*found, *known), (3, 2)),
        other => panic!("expected a refusal, got {other}"),
    }
    let text = err.to_string();
    assert!(text.contains("version 3") && text.contains("up to 2") && text.contains("nothing in it was changed"), "{text}");
    assert_eq!(std::fs::read(&path).unwrap(), before, "byte for byte what it was");
    assert!(!names(&w.root).iter().any(|n| n.contains(".bak-v") || n.contains(".corrupt-") || n.ends_with("-wal")), "{:?}", names(&w.root));
    // `migrate` on an open connection refuses too, and keeps the data.
    let mut conn = Connection::open(&path).unwrap();
    assert!(matches!(migrate(&mut conn, Some(&path), &[STEP1]), Err(StoreError::TooNew { found: 3, known: 1, .. })));
    assert_eq!(count(&conn, "extra"), 1);
}

#[test]
fn upgrading_a_database_that_holds_a_version_keeps_a_copy_and_only_the_two_newest() {
    let w = TempWorld::new("sqlite_backup");
    let path = w.path("store.db");
    drop(open_migrated(&path, &Opts::standard(), &[STEP1]).unwrap());
    assert!(!names(&w.root).iter().any(|n| n.contains(".bak-v")), "no copy when the database is made");
    drop(open_migrated(&path, &Opts::standard(), &[STEP1, STEP2]).unwrap());
    let backup = w.path("store.db.bak-v1");
    assert!(backup.is_file(), "{:?}", names(&w.root));
    // The copy is a database at the old version, with the old rows and without the new column.
    let old = Connection::open(&backup).unwrap();
    assert_eq!(version(&old).unwrap(), 1);
    assert_eq!(count(&old, "notes"), 1);
    assert!(old.prepare("SELECT tag FROM notes").is_err());
    drop(old);
    drop(open_migrated(&path, &Opts::standard(), &[STEP1, STEP2, STEP3]).unwrap());
    let fourth = Migration { id: 4, sql: "CREATE TABLE four (x INTEGER);" };
    drop(open_migrated(&path, &Opts::standard(), &[STEP1, STEP2, STEP3, fourth]).unwrap());
    let kept: Vec<String> = names(&w.root).into_iter().filter(|n| n.contains(".bak-v")).collect();
    assert_eq!(kept, vec!["store.db.bak-v2".to_string(), "store.db.bak-v3".to_string()], "the copy of version 1 was dropped");
    // No upgrade, no new copy.
    let stamp = std::fs::metadata(w.path("store.db.bak-v3")).unwrap().modified().unwrap();
    drop(open_migrated(&path, &Opts::standard(), &[STEP1, STEP2, STEP3, fourth]).unwrap());
    assert_eq!(std::fs::metadata(w.path("store.db.bak-v3")).unwrap().modified().unwrap(), stamp);
}

#[test]
fn a_copy_that_cannot_be_made_stops_the_upgrade_and_leaves_the_database_alone() {
    let w = TempWorld::new("sqlite_backup_fails");
    let path = w.path("store.db");
    drop(open_migrated(&path, &Opts::standard(), &[STEP1]).unwrap());
    // A folder where the copy has to go: the copy cannot be written (and cannot be replaced).
    std::fs::create_dir(w.path("store.db.bak-v1")).unwrap();
    let err = open_migrated(&path, &Opts::standard(), &[STEP1, STEP2]).unwrap_err();
    assert!(matches!(err, StoreError::Backup(_)), "{err}");
    let conn = Connection::open(&path).unwrap();
    assert_eq!(version(&conn).unwrap(), 1, "not upgraded");
    assert!(conn.prepare("SELECT tag FROM notes").is_err());
}

#[test]
fn a_database_made_before_versions_existed_is_stamped_without_running_its_steps_over_it() {
    let w = TempWorld::new("sqlite_legacy");
    let path = w.path("store.db");
    {
        // What the old build made: the tables, the later column already added, and no version.
        let c = Connection::open(&path).unwrap();
        c.execute_batch("CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL, tag TEXT NOT NULL DEFAULT ''); INSERT INTO notes (body, tag) VALUES ('old', 'kept');").unwrap();
    }
    fn adopt(c: &Connection) -> u32 {
        let columns: Vec<String> = c.prepare("PRAGMA table_info(notes)").unwrap().query_map([], |r| r.get::<_, String>(1)).unwrap().map(Result::unwrap).collect();
        if columns.iter().any(|c| c == "tag") {
            2
        } else if columns.iter().any(|c| c == "body") {
            1
        } else {
            0
        }
    }
    // Without `adopt`, step 2 would fail on "duplicate column name" (the old failure); with it the database is stamped at 2 and the steps are skipped.
    let o = open_migrated_adopting(&path, &Opts::standard(), &[STEP1, STEP2, STEP3], adopt).unwrap();
    assert_eq!(version(&o.conn).unwrap(), 3);
    assert_eq!(count(&o.conn, "notes"), 1, "step 1 did not insert its row again");
    let tag: String = o.conn.query_row("SELECT tag FROM notes", [], |r| r.get(0)).unwrap();
    assert_eq!(tag, "kept");
    assert!(has_table(&o.conn, "extra"), "the step after the adopted ones ran");
    assert!(w.path("store.db.bak-v2").is_file(), "an upgrade from an adopted version keeps a copy too");
    // A database with no tables is not adopted: every step runs.
    let w2 = TempWorld::new("sqlite_legacy_empty");
    let o2 = open_migrated_adopting(&w2.path("s.db"), &Opts::standard(), &[STEP1, STEP2], adopt).unwrap();
    assert_eq!(count(&o2.conn, "notes"), 1);
}

#[test]
fn a_list_of_steps_that_does_not_count_up_is_refused() {
    let w = TempWorld::new("sqlite_steps");
    let dup = Migration { id: 1, sql: "SELECT 1" };
    assert!(matches!(open_migrated(&w.path("a.db"), &Opts::standard(), &[STEP1, dup]), Err(StoreError::Steps(_))));
    let zero = Migration { id: 0, sql: "SELECT 1" };
    assert!(matches!(open_migrated(&w.path("b.db"), &Opts::standard(), &[zero]), Err(StoreError::Steps(_))));
    assert!(!w.path("a.db").exists() && !w.path("b.db").exists(), "nothing was made");
}

#[test]
fn four_threads_opening_one_new_database_run_each_step_once() {
    let w = TempWorld::new("sqlite_threads");
    let path = w.path("store.db");
    const LOG1: Migration = Migration { id: 1, sql: "CREATE TABLE log (step INTEGER); INSERT INTO log VALUES (1);" };
    const LOG2: Migration = Migration { id: 2, sql: "INSERT INTO log VALUES (2);" };
    const LOG3: Migration = Migration { id: 3, sql: "INSERT INTO log VALUES (3);" };
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || {
                let o = open_migrated(&path, &Opts::standard().busy(20_000), &[LOG1, LOG2, LOG3]).map_err(|e| e.to_string())?;
                Ok::<_, String>(version(&o.conn).map_err(|e| e.to_string())?)
            })
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap().unwrap(), 3);
    }
    let conn = Connection::open(&path).unwrap();
    let steps: Vec<i64> = conn.prepare("SELECT step FROM log ORDER BY step").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(steps, vec![1, 2, 3], "each step ran exactly once");
}

#[test]
fn four_starts_at_once_that_all_upgrade_the_same_old_database_each_make_a_whole_copy_and_run_the_step_once() {
    // Regression: the copy before an upgrade was written straight to <name>.bak-v<old> by every start; two writers into one file failed with "table already exists" about one run
    // in ten. All four read version 1, all four copy, and only one of them runs each later step.
    let w = TempWorld::new("sqlite_copy_race");
    let path = w.path("store.db");
    const LOG1: Migration = Migration { id: 1, sql: "CREATE TABLE log (step INTEGER); INSERT INTO log VALUES (1);" };
    const LOG2: Migration = Migration { id: 2, sql: "INSERT INTO log VALUES (2);" };
    drop(open_migrated(&path, &Opts::standard(), &[LOG1]).unwrap());
    for round in 0..6 {
        let again = w.path(&format!("round{round}.db"));
        std::fs::copy(&path, &again).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let (again, barrier) = (again.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    let o = open_migrated(&again, &Opts::standard().busy(20_000), &[LOG1, LOG2]).map_err(|e| e.to_string())?;
                    Ok::<_, String>(version(&o.conn).map_err(|e| e.to_string())?)
                })
            })
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap().unwrap(), 2, "round {round}");
        }
        let conn = Connection::open(&again).unwrap();
        assert_eq!(count(&conn, "log"), 2, "the second step ran once, round {round}");
        let copy = Connection::open(sibling(&again, ".bak-v1")).unwrap();
        assert_eq!((version(&copy).unwrap(), count(&copy, "log")), (1, 1), "a whole copy of version 1, round {round}");
        assert!(!names(&w.root).iter().any(|n| n.contains(".part-")), "no half copy is left: {:?}", names(&w.root));
    }
}

#[test]
fn the_private_profile_leaves_no_log_file_and_overwrites_what_is_deleted() {
    let w = TempWorld::new("sqlite_private");
    let path = w.path("facts.db");
    let o = open_migrated(&path, &Opts::private(), &[STEP1]).unwrap();
    o.conn.execute("INSERT INTO notes (body) VALUES ('a secret the person asked to forget')", []).unwrap();
    let mode: String = o.conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
    assert_eq!(mode.to_lowercase(), "delete");
    let secure: i64 = o.conn.query_row("PRAGMA secure_delete", [], |r| r.get(0)).unwrap();
    assert_eq!(secure, 1);
    assert!(!names(&w.root).iter().any(|n| n.ends_with("-wal") || n.ends_with("-shm")), "{:?}", names(&w.root));
    o.conn.execute("DELETE FROM notes", []).unwrap();
    drop(o);
    let bytes = std::fs::read(&path).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("a secret the person asked to forget"), "the deleted text is gone from the file");
}

#[test]
fn the_private_profile_keeps_no_copy_because_a_copy_would_keep_what_the_person_asked_to_forget() {
    let w = TempWorld::new("sqlite_private_no_backup");
    let path = w.path("facts.db");
    drop(open_migrated(&path, &Opts::private(), &[STEP1]).unwrap());
    drop(open_migrated(&path, &Opts::private(), &[STEP1, STEP2]).unwrap());
    assert!(!names(&w.root).iter().any(|n| n.contains(".bak-v")), "{:?}", names(&w.root));
    // The same upgrade under the standard profile does keep one.
    let path2 = w.path("runs.db");
    drop(open_migrated(&path2, &Opts::standard(), &[STEP1]).unwrap());
    drop(open_migrated(&path2, &Opts::standard(), &[STEP1, STEP2]).unwrap());
    assert!(w.path("runs.db.bak-v1").is_file());
}

#[test]
fn a_database_in_a_write_ahead_log_is_rewritten_once_when_it_becomes_private() {
    let w = TempWorld::new("sqlite_wal_to_private");
    let path = w.path("facts.db");
    {
        let o = open_migrated(&path, &Opts::standard(), &[STEP1]).unwrap();
        o.conn.execute("INSERT INTO notes (body) VALUES ('forgotten before the switch')", []).unwrap();
        o.conn.execute("DELETE FROM notes", []).unwrap();
    }
    let o = open_migrated(&path, &Opts::private(), &[STEP1]).unwrap();
    let mode: String = o.conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
    assert_eq!(mode.to_lowercase(), "delete");
    drop(o);
    assert!(!String::from_utf8_lossy(&std::fs::read(&path).unwrap()).contains("forgotten before the switch"));
    assert!(!names(&w.root).iter().any(|n| n.ends_with("-wal")), "{:?}", names(&w.root));
}

#[test]
fn the_light_connection_sets_the_pragmas_of_a_connection_and_nothing_else() {
    let w = TempWorld::new("sqlite_connect");
    let path = w.path("store.db");
    drop(open_migrated(&path, &Opts::standard(), &[STEP1]).unwrap());
    let c = connect(&path, &Opts::standard().busy(7_000).secure_delete(true).foreign_keys(true).synchronous_normal(true)).unwrap();
    let secure: i64 = c.query_row("PRAGMA secure_delete", [], |r| r.get(0)).unwrap();
    let fk: i64 = c.query_row("PRAGMA foreign_keys", [], |r| r.get(0)).unwrap();
    let sync: i64 = c.query_row("PRAGMA synchronous", [], |r| r.get(0)).unwrap();
    let busy: i64 = c.query_row("PRAGMA busy_timeout", [], |r| r.get(0)).unwrap();
    assert_eq!((secure, fk, sync, busy), (1, 1, 1, 7_000));
    // It does not check, move or migrate: a garbage file is an error here, left where it is.
    let junk = w.write("junk.db", "not a database ".repeat(100));
    let c = connect(&junk, &Opts::standard()).unwrap();
    assert!(c.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0)).is_err());
    assert!(junk.is_file() && !names(&w.root).iter().any(|n| n.contains(".corrupt-")));
}

#[test]
fn a_connection_for_one_call_is_refused_when_the_file_is_from_a_newer_build() {
    let w = TempWorld::new("sqlite_connect_checked");
    let path = w.path("store.db");
    drop(open_migrated(&path, &Opts::standard(), &[STEP1, STEP2, STEP3]).unwrap());
    // The build that knows three steps connects; the build that knows two is refused, call by call, and nothing is written.
    assert!(connect_checked(&path, &Opts::standard(), newest(&[STEP1, STEP2, STEP3])).is_ok());
    let before = std::fs::read(&path).unwrap();
    let err = connect_checked(&path, &Opts::standard(), newest(&[STEP1, STEP2])).unwrap_err();
    assert!(matches!(err, StoreError::TooNew { found: 3, known: 2, .. }), "{err}");
    assert_eq!(std::fs::read(&path).unwrap(), before);
    // A file with no version yet (or an empty list of steps) is not refused.
    let w2 = TempWorld::new("sqlite_connect_checked_old");
    assert!(connect_checked(&w2.path("a.db"), &Opts::standard(), newest(&[STEP1])).is_ok());
    assert_eq!(newest(&[]), 0);
}

#[test]
fn leftover_names_are_told_from_look_alikes() {
    for yes in ["a.db.corrupt-1700000000", "a.db.corrupt-1700000000-2", "keys.json.bad-1700000000123", "x.y.db.corrupt-5"] {
        assert!(is_leftover_name(yes), "{yes}");
    }
    for no in ["a.db", "a.db.corrupt-1700000000-wal", "a.db.corrupt-1700000000-shm", "a.db.corrupt-", "a.db.corrupt-abc", ".bad-1", "a.db.bak-v2", "corrupt-5", "notes.bad-1700000000-x"] {
        assert!(!is_leftover_name(no), "{no}");
    }
    let w = TempWorld::new("sqlite_leftovers");
    w.write("a.db", "x");
    let one = w.write("a.db.corrupt-5", "x");
    w.write("a.db.corrupt-5-wal", "x");
    let two = w.write("sub/b.json.bad-1700000000000", "x");
    let deep = w.write("sub/deeper/c.db.corrupt-9", "x");
    w.write("models/m.db.corrupt-1", "x");
    w.write("a/b/c/d/e.db.corrupt-1", "x");
    let mut want = vec![one, two, deep];
    want.sort();
    assert_eq!(leftovers(&w.root), want, "found down to three folders, not in models, not beyond");
    assert!(leftovers(&w.path("missing")).is_empty());
}

#[test]
fn the_error_converts_to_the_string_the_stores_pass_around() {
    let w = TempWorld::new("sqlite_string");
    let bad = Migration { id: 1, sql: "NOT SQL" };
    let text: String = open_migrated(&w.path("s.db"), &Opts::standard(), &[bad]).unwrap_err().into();
    assert!(text.contains("step 1") && text.contains("rolled back"), "{text}");
}

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

use super::sqlite_store::{self, Migration, Opts};
use super::voiceprint;
use aqua_native_tools::secrets::{CredentialManager, Secrets};

/// The schema, one step per change; never edit a step that has shipped, add one at the end (see `storage::sqlite_store`).
/// `task_history` was created here once and never written to: agent runs are recorded in `data/agent_runs.db` (see `agent_runs`). An existing database keeps its empty table; nothing creates it any more.
const STEPS: &[Migration] = &[Migration {
    id: 1,
    sql: "CREATE TABLE IF NOT EXISTS settings (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL,
                 updated_at INTEGER NOT NULL
             );",
}];

#[derive(Clone)]
pub struct DatabaseManager {
    core_db_path: PathBuf,
    solutions_db_path: PathBuf,
    /// Holds the key that seals the voiceprint (the operating system's credential store).
    secrets: Arc<dyn Secrets>,
}

impl DatabaseManager {
    pub fn new<P: AsRef<Path>>(data_dir: P) -> Result<Self> {
        Self::with_secrets(data_dir, Arc::new(CredentialManager))
    }

    /// Like `new`, with a chosen credential store (tests use an in-memory one).
    pub fn with_secrets<P: AsRef<Path>>(data_dir: P, secrets: Arc<dyn Secrets>) -> Result<Self> {
        let data_dir = data_dir.as_ref();
        std::fs::create_dir_all(data_dir)?;

        let core_db_path = data_dir.join("aqua_core.db");
        let solutions_db_path = data_dir.join("solutions.db");

        let manager = Self {
            core_db_path,
            solutions_db_path,
            secrets,
        };

        manager.init_tables()?;
        Ok(manager)
    }

    /// Write-ahead log for concurrency, `synchronous = NORMAL`. **A damaged file is an error and stays where it is** (`recover(false)`): this file holds the voice lock and
    /// the voiceprint, and a store that quietly started again empty would read as "no voice enrolled, nothing to lock" (see `voice_lock_enabled`, which fails closed on an error).
    fn opts() -> Opts {
        Opts::standard().synchronous_normal(true).recover(false)
    }

    fn init_tables(&self) -> Result<()> {
        sqlite_store::open_migrated(&self.core_db_path, &Self::opts(), STEPS).context("Failed to open aqua_core.db database")?;
        info!("SQLite WAL Database initialized at {:?}", self.core_db_path);
        Ok(())
    }

    /// A connection for one call; a file from a newer build is refused.
    fn conn(&self) -> Result<rusqlite::Connection> {
        Ok(sqlite_store::connect_checked(&self.core_db_path, &Self::opts(), sqlite_store::newest(STEPS))?)
    }

    pub fn solutions_db_path(&self) -> &Path {
        &self.solutions_db_path
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();

        conn.execute(
            "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = ?2, updated_at = ?3",
            (key, value, now),
        )?;

        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT value FROM settings WHERE key = ?1")?;
        let mut rows = stmt.query([key])?;

        if let Some(row) = rows.next()? {
            let val: String = row.get(0)?;
            Ok(Some(val))
        } else {
            Ok(None)
        }
    }

    /// Is the voice lock on? What the person chose wins (`voice_security_enabled` set to `true` or `false`). When they have not
    /// chosen, the lock is on exactly when a voiceprint exists: with no print there is no one to recognise, and a lock that is on
    /// refuses every voice request without a word (a fresh install did exactly that). A lock that protects a voice is never
    /// loosened: an enrolled voice keeps it on unless the person turned it off.
    ///
    /// **It fails closed.** If the database cannot be read, the lock is on: an error must never be the way a protected install ends up
    /// accepting any voice (the old default was "on", and this keeps that for every case where the answer is not known).
    pub fn voice_lock_enabled(&self) -> bool {
        match self.get_setting("voice_security_enabled") {
            Ok(Some(v)) if v.trim() == "true" => true,
            Ok(Some(v)) if v.trim() == "false" => false,
            Ok(_) => self.master_voice_stored().unwrap_or(true),
            Err(_) => true,
        }
    }

    /// Has a voice been enrolled? (Looks only at whether a print is stored; it does not unseal it.) `None` when the database cannot be read.
    fn master_voice_stored(&self) -> Option<bool> {
        let embedding = self.get_setting("master_voice_embedding").ok()?;
        let enrolled = self.get_setting("master_voice_enrolled").ok()?;
        Some(embedding.is_some_and(|v| !v.trim().is_empty()) || enrolled.is_some_and(|v| v == "true" || v == "1"))
    }

    /// Has a voice been enrolled? `false` when that cannot be found out (the lock does not use this: see `voice_lock_enabled`).
    pub fn has_master_voice(&self) -> bool {
        self.master_voice_stored().unwrap_or(false)
    }

    pub fn save_master_voice_profile(&self, embedding: &[f32], wakeup_word: &str, custom_reply: &str) -> Result<()> {
        // Never stored as readable numbers; if the credential store cannot hold the key, nothing is saved.
        let sealed = voiceprint::protect(self.secrets.as_ref(), embedding).map_err(|e| anyhow::anyhow!("could not protect the voiceprint: {e}"))?;
        self.set_setting("master_voice_embedding", &sealed)?;
        self.set_setting("master_voice_enrolled", "true")?;
        self.set_setting("wakeup_word", wakeup_word)?;
        self.set_setting("custom_reply", custom_reply)?;
        Ok(())
    }

    pub fn get_master_voice_embedding(&self) -> Result<Option<Vec<f32>>> {
        if let Some(val) = self.get_setting("master_voice_embedding")? {
            let emb = voiceprint::reveal(self.secrets.as_ref(), &val).map_err(|e| anyhow::anyhow!(e))?;
            // A voiceprint written by an older version is plain JSON: seal it now.
            if !voiceprint::is_sealed(&val) {
                match voiceprint::protect(self.secrets.as_ref(), &emb) {
                    Ok(sealed) => {
                        if self.set_setting("master_voice_embedding", &sealed).is_ok() {
                            info!("The stored voiceprint was plain text and is now encrypted.");
                        }
                    }
                    Err(e) => warn!("The stored voiceprint is still plain text: {e}"),
                }
            }
            Ok(Some(emb))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod voiceprint_tests {
    use super::*;
    use aqua_native_tools::secrets::MemorySecrets;

    fn db(name: &str, secrets: Arc<dyn Secrets>) -> DatabaseManager {
        let dir = std::env::temp_dir().join(format!("aqua_db_vp_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        DatabaseManager::with_secrets(&dir, secrets).unwrap()
    }

    #[test]
    fn the_database_holds_the_voiceprint_sealed_and_gives_it_back_unchanged() {
        let d = db("sealed", Arc::new(MemorySecrets::new()));
        let emb = vec![0.111_f32, -0.222, 0.333];
        d.save_master_voice_profile(&emb, "Hey Aqua", "Yes?").unwrap();
        let raw = d.get_setting("master_voice_embedding").unwrap().unwrap();
        assert!(voiceprint::is_sealed(&raw) && !raw.contains("0.111"), "{raw}");
        assert_eq!(d.get_master_voice_embedding().unwrap().unwrap(), emb);
    }

    #[test]
    fn an_old_plain_voiceprint_still_loads_and_is_sealed_in_place() {
        let d = db("migrate", Arc::new(MemorySecrets::new()));
        d.set_setting("master_voice_embedding", "[0.5,0.25]").unwrap();
        assert_eq!(d.get_master_voice_embedding().unwrap().unwrap(), vec![0.5, 0.25]);
        assert!(voiceprint::is_sealed(&d.get_setting("master_voice_embedding").unwrap().unwrap()), "upgraded after being read");
        assert_eq!(d.get_master_voice_embedding().unwrap().unwrap(), vec![0.5, 0.25]);
    }

    #[test]
    fn the_voice_lock_is_on_by_default_only_when_a_voice_has_been_enrolled() {
        let d = db("lock_default", Arc::new(MemorySecrets::new()));
        // A fresh install: nobody is enrolled, so there is no one to recognise and nothing to lock.
        assert!(!d.has_master_voice() && !d.voice_lock_enabled());
        // The first enrolment turns the protection on by itself.
        d.save_master_voice_profile(&[0.1, 0.2, 0.3], "Hey Aqua", "Yes?").unwrap();
        assert!(d.has_master_voice() && d.voice_lock_enabled());
    }

    #[test]
    fn a_database_that_cannot_be_read_keeps_the_voice_lock_on() {
        let dir = std::env::temp_dir().join(format!("aqua_db_vp_unreadable_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let d = DatabaseManager::with_secrets(&dir, Arc::new(MemorySecrets::new())).unwrap();
        // Nobody enrolled, nothing chosen: the lock is off while the database can be read...
        assert!(!d.voice_lock_enabled());
        // ...but if it cannot be read, "not enrolled" is not something Aqua knows, so the lock stays on.
        for f in ["aqua_core.db", "aqua_core.db-wal", "aqua_core.db-shm"] {
            let _ = std::fs::remove_file(dir.join(f));
        }
        std::fs::create_dir_all(dir.join("aqua_core.db")).unwrap(); // a folder where the database file should be
        assert!(d.get_setting("voice_security_enabled").is_err(), "the read really fails");
        assert!(d.voice_lock_enabled(), "an unreadable database must not turn the lock off");
        assert!(!d.has_master_voice(), "while the plain question still answers no");
    }

    #[test]
    fn what_the_person_chose_for_the_voice_lock_always_wins() {
        let d = db("lock_choice", Arc::new(MemorySecrets::new()));
        // An existing lock stays on: with no print, a stored "true" is respected (the person set it).
        d.set_setting("voice_security_enabled", "true").unwrap();
        assert!(d.voice_lock_enabled());
        // Turned off on purpose, it stays off even with a voice enrolled.
        d.save_master_voice_profile(&[0.1], "Hey Aqua", "Yes?").unwrap();
        d.set_setting("voice_security_enabled", "false").unwrap();
        assert!(!d.voice_lock_enabled());
        // An unreadable or blank value is not a choice: the print decides.
        d.set_setting("voice_security_enabled", "maybe").unwrap();
        assert!(d.voice_lock_enabled());
        // A plain voiceprint written by an older version counts as enrolled, too.
        let old = db("lock_old_print", Arc::new(MemorySecrets::new()));
        old.set_setting("master_voice_embedding", "[0.5,0.25]").unwrap();
        assert!(old.voice_lock_enabled(), "a voice that was enrolled before keeps its lock");
    }

    #[test]
    fn a_credential_store_that_cannot_hold_the_key_means_nothing_is_saved() {
        struct Broken;
        impl Secrets for Broken {
            fn get(&self, _: &str) -> Result<Option<String>, String> { Err("store unavailable".into()) }
            fn set(&self, _: &str, _: &str) -> Result<(), String> { Err("store unavailable".into()) }
            fn delete(&self, _: &str) -> Result<bool, String> { Err("store unavailable".into()) }
        }
        let d = db("broken", Arc::new(Broken));
        let err = d.save_master_voice_profile(&[1.0], "Hey Aqua", "Yes?").unwrap_err().to_string();
        assert!(err.contains("could not protect the voiceprint"), "{err}");
        assert!(d.get_setting("master_voice_embedding").unwrap().is_none(), "no plain copy is written as a fallback");
    }
}

#[cfg(test)]
mod file_tests {
    use super::*;
    use crate::test_support::TempWorld;
    use aqua_native_tools::secrets::MemorySecrets;

    fn version(path: &Path) -> i64 {
        rusqlite::Connection::open(path).unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn a_new_settings_file_has_a_version_and_one_from_before_versions_is_stamped_and_keeps_its_rows() {
        let w = TempWorld::new("core_db_version");
        let d = DatabaseManager::with_secrets(&w.root, Arc::new(MemorySecrets::new())).unwrap();
        d.set_setting("system_status", "ACTIVE").unwrap();
        let path = w.path("aqua_core.db");
        assert_eq!(version(&path), 1);
        rusqlite::Connection::open(&path).unwrap().execute_batch("PRAGMA user_version = 0;").unwrap();
        let again = DatabaseManager::with_secrets(&w.root, Arc::new(MemorySecrets::new())).unwrap();
        assert_eq!(version(&path), 1);
        assert_eq!(again.get_setting("system_status").unwrap().as_deref(), Some("ACTIVE"));
    }

    #[test]
    fn a_damaged_settings_file_is_an_error_and_stays_where_it_is_so_the_voice_lock_does_not_start_again_empty() {
        let w = TempWorld::new("core_db_damaged");
        let path = w.write("aqua_core.db", "this is not a database, only text. ".repeat(300));
        let err = DatabaseManager::with_secrets(&w.root, Arc::new(MemorySecrets::new())).err().expect("an error, not a new empty file");
        assert!(format!("{err:#}").contains("damaged") || format!("{err:#}").contains("not a database"), "{err:#}");
        assert!(std::fs::read_to_string(&path).unwrap().starts_with("this is not a database"), "untouched");
        assert!(crate::storage::sqlite_store::leftovers(&w.root).is_empty(), "nothing was moved aside");
    }

    #[test]
    fn a_settings_file_from_a_newer_build_is_refused_on_every_call() {
        let w = TempWorld::new("core_db_newer");
        let d = DatabaseManager::with_secrets(&w.root, Arc::new(MemorySecrets::new())).unwrap();
        d.set_setting("k", "v").unwrap();
        {
            let c = rusqlite::Connection::open(w.path("aqua_core.db")).unwrap();
            let _: String = c.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0)).unwrap();
            c.execute_batch("PRAGMA user_version = 5;").unwrap();
        }
        let before = std::fs::read(w.path("aqua_core.db")).unwrap();
        let err = d.get_setting("k").unwrap_err();
        assert!(format!("{err:#}").contains("newer than this build"), "{err:#}");
        assert!(d.set_setting("k", "changed").is_err());
        assert!(d.voice_lock_enabled(), "a file that cannot be used keeps the voice lock on");
        assert_eq!(std::fs::read(w.path("aqua_core.db")).unwrap(), before);
    }
}

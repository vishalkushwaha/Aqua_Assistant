//! Where what the person asked Aqua to remember lives: `data/user_memory.db`.
//!
//! A small typed store, written only on purpose: by the person's own words ("remember that ..."), by the person in the app, or by a tool the
//! person was asked about. A fact is one short line. Secrets are refused (a password said aloud is not kept), the store is capped, every fact
//! can be seen, exported and deleted, and deleting is deleting (the row goes, it is not marked).

use crate::storage::sqlite_store::{self, Migration, Opts};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

/// The schema, one step per change; never edit a step that has shipped, add one at the end (see `storage::sqlite_store`). A file made before versions existed holds these
/// tables already: the step does nothing to it and the version is stamped.
const STEPS: &[Migration] = &[Migration {
    id: 1,
    sql: "CREATE TABLE IF NOT EXISTS facts (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 kind TEXT NOT NULL,
                 text TEXT NOT NULL,
                 norm TEXT NOT NULL,
                 source TEXT NOT NULL,
                 created INTEGER NOT NULL,
                 last_used INTEGER NOT NULL DEFAULT 0,
                 uses INTEGER NOT NULL DEFAULT 0,
                 expires INTEGER,
                 sensitive INTEGER NOT NULL DEFAULT 0
             );
             CREATE UNIQUE INDEX IF NOT EXISTS idx_facts_norm ON facts(norm);
             CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS proposals (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 kind TEXT NOT NULL,
                 fact_ids TEXT NOT NULL,
                 keep_id INTEGER,
                 new_text TEXT NOT NULL,
                 norm TEXT NOT NULL,
                 reason TEXT NOT NULL,
                 created INTEGER NOT NULL,
                 status TEXT NOT NULL DEFAULT 'pending',
                 decided INTEGER NOT NULL DEFAULT 0
             );
             CREATE UNIQUE INDEX IF NOT EXISTS idx_proposals_norm ON proposals(norm);",
}];

/// The most facts kept. A person who has more than this needs a notes file, not a prompt.
pub const MAX_FACTS: i64 = 200;
/// The longest a fact may be, in characters.
pub const MAX_CHARS: usize = 300;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Fact {
    pub id: i64,
    /// `fact` or `preference`.
    pub kind: String,
    pub text: String,
    /// Who wrote it: `user` (their own words), `ui` (the app) or `tool` (a tool call the person approved).
    pub source: String,
    pub created: i64,
    pub last_used: i64,
    pub uses: i64,
    /// A fact about health, money, identity papers and the like: never put into a prompt on its own, only shown to the person.
    pub sensitive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    Empty,
    TooLong,
    /// The text holds something that looks like a password, key or token. Said so, and not kept.
    LooksLikeASecret,
    /// The store is full.
    Full,
    Incognito,
    /// This many suggestions are waiting for the person already.
    QueueFull(i64),
    Db(String),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::Empty => f.write_str("there is nothing to remember in that"),
            WriteError::TooLong => write!(f, "that is too long to remember (at most {MAX_CHARS} characters)"),
            WriteError::LooksLikeASecret => f.write_str("that looks like a password or a key, and I do not keep those"),
            WriteError::Full => write!(f, "my memory of you is full ({MAX_FACTS} things): forget something first"),
            WriteError::Incognito => f.write_str("I am not remembering anything right now (incognito)"),
            WriteError::QueueFull(n) => write!(f, "{n} suggestions are already waiting for you to look at: decide on those first"),
            WriteError::Db(e) => write!(f, "the memory could not be written: {e}"),
        }
    }
}

pub struct FactStore {
    conn: Mutex<Connection>,
    /// Set when the person asked for incognito and the setting could not be saved: this process stays closed anyway.
    forced_incognito: AtomicBool,
}

fn db(e: rusqlite::Error) -> WriteError {
    WriteError::Db(e.to_string())
}

/// Logs that a write to the memory file (or a read of the privacy setting) failed, but at most once a minute: the setting is read on every prompt and a full disk would otherwise
/// fill the log.
fn warn_write_failed(what: &str, why: &dyn std::fmt::Display) {
    static LAST_LOGGED_SECS: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let last = LAST_LOGGED_SECS.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= 60 && LAST_LOGGED_SECS.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
        tracing::warn!("memory: {what}: {why}");
    }
}

/// What a fact looks like once whitespace is squeezed and case and edge punctuation are gone, for telling two phrasings of one thing apart.
pub fn normalise(text: &str) -> String {
    let squeezed = text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    squeezed.trim_matches(|c: char| !c.is_alphanumeric()).to_string()
}

const SENSITIVE_WORDS: &[&str] = &[
    "health", "medical", "medication", "diagnos", "disease", "cancer", "pregnan", "therap", "depress", "religio", "sexual", "passport", "aadhaar", "social security", "ssn", "bank account", "salary", "income", "credit", "debt", "tax",
];

fn is_sensitive(text: &str) -> bool {
    let t = text.to_lowercase();
    SENSITIVE_WORDS.iter().any(|w| t.contains(w))
}

fn kind_of(text: &str) -> &'static str {
    let t = normalise(text);
    ["i prefer", "i like", "i don't like", "i dont like", "i do not like", "i hate", "i want you to", "always ", "never ", "call me", "address me", "speak to me", "answer in", "reply in", "keep answers"]
        .iter()
        .any(|p| t.starts_with(p))
        .then_some("preference")
        .unwrap_or("fact")
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Fact> {
    Ok(Fact { id: r.get(0)?, kind: r.get(1)?, text: r.get(2)?, source: r.get(3)?, created: r.get(4)?, last_used: r.get(5)?, uses: r.get(6)?, sensitive: r.get::<_, i64>(7)? != 0 })
}

const COLUMNS: &str = "id, kind, text, source, created, last_used, uses, sensitive";

/// A change that the memory suggests and the person has not decided on (see [`super::dream`]), or something that a correction taught. **Nothing here is a fact** until the person
/// accepts it, and accepting is one of the person's doors: a suggestion is never written by itself, and nothing the model reads comes from this table.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Proposal {
    pub id: i64,
    /// `merge` (a fact that another already says: forget it, keep the other), `dates` (a fact with a relative date, written with the real date) or `lesson` (a correction the
    /// person made, to be remembered as a fact of its own kind).
    pub kind: String,
    /// The facts it is about: what a merge forgets, the one a date change rewrites. Empty for a lesson.
    pub fact_ids: Vec<i64>,
    /// The fact a merge keeps.
    pub keep_id: Option<i64>,
    /// What the fact will say: the text that is kept, the text as rewritten, the lesson.
    pub reason: String,
    pub new_text: String,
    pub created: i64,
    /// `pending`, `accepted` or `rejected`.
    pub status: String,
    pub decided: i64,
}

/// The most suggestions that wait at once: a person who has this many has not been looking, and more would only bury them.
pub const MAX_PENDING_PROPOSALS: i64 = 50;
const PROPOSAL_COLUMNS: &str = "id, kind, fact_ids, keep_id, new_text, reason, created, status, decided";

fn proposal_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Proposal> {
    let ids: String = r.get(2)?;
    Ok(Proposal {
        id: r.get(0)?,
        kind: r.get(1)?,
        fact_ids: serde_json::from_str::<Vec<i64>>(&ids).unwrap_or_default(),
        keep_id: r.get(3)?,
        new_text: r.get(4)?,
        reason: r.get(5)?,
        created: r.get(6)?,
        status: r.get(7)?,
        decided: r.get(8)?,
    })
}

/// A text as one line: control characters are spaces and whitespace is squeezed.
fn one_line_of(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
}

impl FactStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        // What is forgotten has to be gone from the file: a write-ahead log keeps a second copy of every page, and a deleted row stays in its page until something
        // overwrites it. So no log (the same choice as the vault), `secure_delete`, and a database that was in the old mode is rewritten once (the private profile does all three).
        let opened = sqlite_store::open_migrated(path, &Opts::private(), STEPS).map_err(|e| e.to_string())?;
        let c = opened.conn;
        if let Some(moved) = &opened.recovered_from {
            // The old file held the person's choice about incognito and it cannot be read. A rebuilt store starts with nothing remembered until the person says otherwise.
            tracing::warn!("the file of remembered facts was damaged and was moved to {}; a new one was made and it starts in incognito (nothing is remembered) until you turn that off", moved.display());
            c.execute("INSERT INTO settings (key, value) VALUES ('incognito', '1') ON CONFLICT(key) DO UPDATE SET value = '1'", []).map_err(|e| e.to_string())?;
        }
        Ok(Self { conn: Mutex::new(c), forced_incognito: AtomicBool::new(false) })
    }

    fn c(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Makes every later write fail as it does on a file that is read-only or locked (tests of what happens then).
    #[cfg(test)]
    pub(super) fn read_only_for_test(&self) {
        self.c().execute_batch("PRAGMA query_only = ON").unwrap();
    }

    /// Whether Aqua is incognito. **Fails closed**: when the setting cannot be read (the file is locked, the table is gone, the value is not one this program wrote) the answer is
    /// `true`, because "I could not tell" must never mean "remember". No row at all is the normal state of a memory that was never switched: off.
    pub fn incognito(&self) -> bool {
        if self.forced_incognito.load(Ordering::SeqCst) {
            return true;
        }
        match self.c().query_row("SELECT value FROM settings WHERE key = 'incognito'", [], |r| r.get::<_, String>(0)).optional() {
            Ok(None) => false,
            Ok(Some(v)) if v == "0" => false,
            Ok(Some(v)) if v == "1" => true,
            Ok(Some(_)) => {
                warn_write_failed("the incognito setting holds a value this program did not write, so incognito is assumed", &"unreadable value");
                true
            }
            Err(e) => {
                warn_write_failed("the incognito setting could not be read, so incognito is assumed", &e);
                true
            }
        }
    }

    /// Switches incognito on or off and says whether that was saved. When turning it **on** cannot be saved the memory still stays closed until Aqua restarts (the person asked for it,
    /// and a promise not to remember must not depend on a write that worked); when turning it **off** cannot be saved it stays on.
    pub fn set_incognito(&self, on: bool) -> Result<(), String> {
        let saved = self.c().execute("INSERT INTO settings (key, value) VALUES ('incognito', ?1) ON CONFLICT(key) DO UPDATE SET value = ?1", [if on { "1" } else { "0" }]);
        match saved {
            Ok(_) => {
                self.forced_incognito.store(false, Ordering::SeqCst);
                Ok(())
            }
            Err(e) => {
                if on {
                    self.forced_incognito.store(true, Ordering::SeqCst);
                    Err(format!("the incognito setting could not be saved ({e}); I will not remember anything until Aqua restarts"))
                } else {
                    Err(format!("the incognito setting could not be saved ({e}); incognito stays on"))
                }
            }
        }
    }

    /// Keeps one fact. The same thing said again (in other words: case, spacing, a full stop) is the same fact: it is touched, not added twice.
    pub fn remember(&self, text: &str, source: &str, now: i64) -> Result<Fact, WriteError> {
        self.remember_kind(text, source, now, None)
    }

    /// `remember` with the kind given (`lesson`, for what a correction taught) instead of read from the words.
    pub(super) fn remember_kind(&self, text: &str, source: &str, now: i64, kind: Option<&str>) -> Result<Fact, WriteError> {
        if self.incognito() {
            return Err(WriteError::Incognito);
        }
        let one_line: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
        if one_line.is_empty() {
            return Err(WriteError::Empty);
        }
        if one_line.chars().count() > MAX_CHARS {
            return Err(WriteError::TooLong);
        }
        // Anything the redactor would mask is a secret; the text is refused as it is, never stored masked.
        if aqua_tool_host::output::Redactor::new().redact(&one_line) != one_line {
            return Err(WriteError::LooksLikeASecret);
        }
        let norm = normalise(&one_line);
        let c = self.c();
        if let Some(existing) = c.query_row(&format!("SELECT {COLUMNS} FROM facts WHERE norm = ?1"), [&norm], row).optional().map_err(db)? {
            c.execute("UPDATE facts SET last_used = ?2 WHERE id = ?1", params![existing.id, now]).map_err(db)?;
            return Ok(existing);
        }
        let count: i64 = c.query_row("SELECT COUNT(*) FROM facts", [], |r| r.get(0)).map_err(db)?;
        if count >= MAX_FACTS {
            return Err(WriteError::Full);
        }
        c.execute(
            "INSERT INTO facts (kind, text, norm, source, created, sensitive) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![kind.unwrap_or_else(|| kind_of(&one_line)), one_line, norm, source, now, is_sensitive(&one_line) as i64],
        )
        .map_err(db)?;
        let id = c.last_insert_rowid();
        c.query_row(&format!("SELECT {COLUMNS} FROM facts WHERE id = ?1"), [id], row).map_err(db)
    }

    /// The facts, newest first. `sensitive` ones are included only when asked for (a prompt never asks).
    pub fn list(&self, include_sensitive: bool, limit: usize) -> Vec<Fact> {
        let c = self.c();
        let mut st = match c.prepare(&format!("SELECT {COLUMNS} FROM facts WHERE (?1 = 1 OR sensitive = 0) ORDER BY created DESC, id DESC LIMIT ?2")) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        st.query_map(params![include_sensitive as i64, limit as i64], row).map(|r| r.filter_map(Result::ok).collect()).unwrap_or_default()
    }

    pub fn count(&self) -> i64 {
        self.c().query_row("SELECT COUNT(*) FROM facts", [], |r| r.get(0)).unwrap_or(0)
    }

    pub fn get(&self, id: i64) -> Option<Fact> {
        self.c().query_row(&format!("SELECT {COLUMNS} FROM facts WHERE id = ?1"), [id], row).optional().ok().flatten()
    }

    /// Deletes one fact and rewrites the file without it. `true` when there was one.
    pub fn forget(&self, id: i64) -> bool {
        let gone = self.delete_row(id);
        if gone {
            self.compact();
        }
        gone
    }

    fn delete_row(&self, id: i64) -> bool {
        let text = self.get(id).map(|f| normalise(&f.text));
        let gone = self.c().execute("DELETE FROM facts WHERE id = ?1", [id]).map(|n| n > 0).unwrap_or(false);
        if gone {
            self.drop_proposals_about(id, text);
        }
        gone
    }

    /// A forgotten fact is gone from the suggestions as well (they hold copies of its words): those that were about it, and those whose text is its own. Forgetting is forgetting.
    fn drop_proposals_about(&self, id: i64, norm: Option<String>) {
        let c = self.c();
        let rows: Vec<(i64, String, Option<i64>, String)> = match c.prepare("SELECT id, fact_ids, keep_id, new_text FROM proposals") {
            Ok(mut st) => st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).map(|r| r.filter_map(Result::ok).collect()).unwrap_or_default(),
            Err(_) => return,
        };
        for (pid, ids, keep, new_text) in rows {
            let about = serde_json::from_str::<Vec<i64>>(&ids).map(|v| v.contains(&id)).unwrap_or(false) || keep == Some(id);
            let same_words = norm.as_ref().is_some_and(|n| *n == normalise(&new_text));
            if about || same_words {
                if let Err(e) = c.execute("DELETE FROM proposals WHERE id = ?1", [pid]) {
                    warn_write_failed("a suggestion about a forgotten fact could not be deleted", &e);
                }
            }
        }
    }

    /// Rewrites the database file, so that what was deleted is not left in it (`secure_delete` overwrites the rows, this drops the free pages).
    fn compact(&self) {
        let c = self.c();
        if let Err(e) = c.execute_batch("PRAGMA secure_delete = ON; VACUUM") {
            tracing::warn!("the memory file could not be compacted after a delete: {e}");
        }
    }

    /// Deletes the facts that are what `query` says to forget: all of its words are in the fact (so "my sister" forgets "my sister is Asha" and
    /// not "my brother is Ravi"). Returns what was deleted.
    pub fn forget_matching(&self, query: &str) -> Vec<Fact> {
        let words = super::retrieve::tokens(query);
        if words.is_empty() {
            return Vec::new();
        }
        let hits: Vec<Fact> = self.list(true, MAX_FACTS as usize).into_iter().filter(|f| {
            let have = super::retrieve::tokens(&f.text);
            words.iter().all(|w| have.contains(w))
        }).collect();
        for f in &hits {
            self.delete_row(f.id);
        }
        if !hits.is_empty() {
            self.compact();
        }
        hits
    }

    /// Notes that these facts were put in front of the model.
    pub fn touch(&self, ids: &[i64], now: i64) {
        let c = self.c();
        for id in ids {
            if let Err(e) = c.execute("UPDATE facts SET uses = uses + 1, last_used = ?2 WHERE id = ?1", params![id, now]) {
                warn_write_failed("the use of a fact could not be noted", &e);
                break;
            }
        }
    }

    /// Deletes everything. Returns how many facts there were.
    pub fn clear(&self) -> usize {
        let n = self.c().execute("DELETE FROM facts", []).unwrap_or(0);
        let waiting = self.c().execute("DELETE FROM proposals", []).unwrap_or(0);
        if n > 0 || waiting > 0 {
            self.compact();
        }
        n
    }

    /// Puts a suggestion in the queue for the person to look at. `Ok(None)` when it is one that was made before (decided or not: a "no" is not asked again). Words that look like a
    /// secret are refused as they are for a fact.
    pub fn add_proposal(&self, kind: &str, fact_ids: &[i64], keep_id: Option<i64>, new_text: &str, reason: &str, now: i64) -> Result<Option<Proposal>, WriteError> {
        if self.incognito() {
            return Err(WriteError::Incognito);
        }
        if !matches!(kind, "merge" | "dates" | "lesson") {
            return Err(WriteError::Db(format!("{kind} is not a kind of suggestion")));
        }
        let text = one_line_of(new_text);
        if text.is_empty() {
            return Err(WriteError::Empty);
        }
        if text.chars().count() > MAX_CHARS {
            return Err(WriteError::TooLong);
        }
        if aqua_tool_host::output::Redactor::new().redact(&text) != text {
            return Err(WriteError::LooksLikeASecret);
        }
        let norm = normalise(&format!("{kind} {fact_ids:?} {keep_id:?} {text}"));
        let reason: String = one_line_of(reason).chars().take(MAX_CHARS).collect();
        let c = self.c();
        let known: Option<i64> = c.query_row("SELECT id FROM proposals WHERE norm = ?1", [&norm], |r| r.get(0)).optional().map_err(db)?;
        if known.is_some() {
            return Ok(None);
        }
        let waiting: i64 = c.query_row("SELECT COUNT(*) FROM proposals WHERE status = 'pending'", [], |r| r.get(0)).map_err(db)?;
        if waiting >= MAX_PENDING_PROPOSALS {
            return Err(WriteError::QueueFull(MAX_PENDING_PROPOSALS));
        }
        let ids = serde_json::to_string(fact_ids).unwrap_or_else(|_| "[]".into());
        c.execute(
            "INSERT INTO proposals (kind, fact_ids, keep_id, new_text, norm, reason, created) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![kind, ids, keep_id, text, norm, reason, now],
        )
        .map_err(db)?;
        let id = c.last_insert_rowid();
        c.query_row(&format!("SELECT {PROPOSAL_COLUMNS} FROM proposals WHERE id = ?1"), [id], proposal_row).optional().map_err(db)
    }

    /// The suggestions with a status (`pending`, `accepted`, `rejected`) or all of them, newest first.
    pub fn proposals(&self, status: Option<&str>, limit: usize) -> Vec<Proposal> {
        let c = self.c();
        let mut st = match c.prepare(&format!("SELECT {PROPOSAL_COLUMNS} FROM proposals WHERE (?1 = '' OR status = ?1) ORDER BY created DESC, id DESC LIMIT ?2")) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        st.query_map(params![status.unwrap_or(""), limit as i64], proposal_row).map(|r| r.filter_map(Result::ok).collect()).unwrap_or_default()
    }

    pub fn proposal(&self, id: i64) -> Option<Proposal> {
        self.c().query_row(&format!("SELECT {PROPOSAL_COLUMNS} FROM proposals WHERE id = ?1"), [id], proposal_row).optional().ok().flatten()
    }

    /// Marks a suggestion decided. A failure is returned: a "no" that was not saved would be asked again, and a "yes" that was not saved would look undone.
    fn set_proposal_status(&self, id: i64, status: &str, now: i64) -> Result<(), String> {
        self.c().execute("UPDATE proposals SET status = ?2, decided = ?3 WHERE id = ?1", params![id, status, now]).map(|_| ()).map_err(|e| format!("the decision on suggestion {id} could not be saved: {e}"))
    }

    /// A fact written again in other words (a date made real): the same fact, with its history, and its text changed. When the new words are a fact that is already there, this one
    /// was a duplicate and goes.
    pub(super) fn rewrite(&self, id: i64, new_text: &str) -> Result<Fact, WriteError> {
        if self.incognito() {
            return Err(WriteError::Incognito);
        }
        let text = one_line_of(new_text);
        if text.is_empty() {
            return Err(WriteError::Empty);
        }
        if text.chars().count() > MAX_CHARS {
            return Err(WriteError::TooLong);
        }
        if aqua_tool_host::output::Redactor::new().redact(&text) != text {
            return Err(WriteError::LooksLikeASecret);
        }
        let norm = normalise(&text);
        let other: Option<Fact> = self.c().query_row(&format!("SELECT {COLUMNS} FROM facts WHERE norm = ?1"), [&norm], row).optional().map_err(db)?;
        if let Some(other) = other {
            if other.id != id {
                self.forget(id);
            }
            return Ok(other);
        }
        let changed = self.c().execute("UPDATE facts SET text = ?2, norm = ?3, sensitive = ?4 WHERE id = ?1", params![id, text, norm, is_sensitive(&text) as i64]).map_err(db)?;
        if changed == 0 {
            return Err(WriteError::Db("the fact is gone".into()));
        }
        // The old words must not stay in the file.
        self.compact();
        self.get(id).ok_or_else(|| WriteError::Db("the fact is gone".into()))
    }

    /// The person's decision on a suggestion. A "no" is kept (so that it is not made again). A "yes" is the person's own write, as `source`: a merge forgets the fact that another
    /// already says, a date change rewrites the fact in place, a lesson becomes a fact of the kind `lesson`. A suggestion about facts that are gone or changed is dropped, not done.
    pub fn decide_proposal(&self, id: i64, accept: bool, source: &str, now: i64) -> Result<Proposal, String> {
        let mut p = self.proposal(id).ok_or_else(|| format!("there is no suggestion {id}"))?;
        if p.status != "pending" {
            return Err(format!("suggestion {id} was already {}", p.status));
        }
        if !accept {
            self.set_proposal_status(id, "rejected", now)?;
            p.status = "rejected".into();
            p.decided = now;
            return Ok(p);
        }
        let gone = p.fact_ids.iter().chain(p.keep_id.iter()).any(|f| self.get(*f).is_none());
        if gone {
            self.set_proposal_status(id, "rejected", now)?;
            return Err("the facts this was about have changed since, so the suggestion is dropped".into());
        }
        match p.kind.as_str() {
            "merge" => {
                for old in &p.fact_ids {
                    self.forget(*old);
                }
            }
            "dates" => {
                let Some(fact) = p.fact_ids.first() else { return Err("the suggestion names no fact".into()) };
                self.rewrite(*fact, &p.new_text).map_err(|e| e.to_string())?;
            }
            "lesson" => {
                self.remember_kind(&p.new_text, source, now, Some("lesson")).map_err(|e| e.to_string())?;
            }
            other => return Err(format!("{other} is not a kind of suggestion")),
        }
        // (A merge or a date change that took its facts' rows with it has no row left to mark: what it was about is gone, which is what was decided.)
        self.set_proposal_status(id, "accepted", now).map_err(|e| format!("the change was made, but {e}"))?;
        p.status = "accepted".into();
        p.decided = now;
        Ok(p)
    }

    /// Every fact, for the person to keep.
    pub fn export(&self) -> Value {
        json!({ "facts": self.list(true, MAX_FACTS as usize), "incognito": self.incognito() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> FactStore {
        let dir = std::env::temp_dir().join(format!("aqua_user_facts_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        FactStore::open(dir.join("user_memory.db")).unwrap()
    }

    #[test]
    fn a_fact_is_kept_as_one_line_with_its_kind_and_who_wrote_it() {
        let s = store("keep");
        let f = s.remember("  My sister's name\nis Asha.  ", "user", 100).unwrap();
        assert_eq!((f.text.as_str(), f.kind.as_str(), f.source.as_str(), f.sensitive), ("My sister's name is Asha.", "fact", "user", false));
        assert_eq!(s.remember("I prefer short answers", "user", 101).unwrap().kind, "preference");
        assert_eq!(s.remember("Call me Vish", "ui", 102).unwrap().kind, "preference");
        assert_eq!(s.count(), 3);
        assert_eq!(s.list(false, 10)[0].text, "Call me Vish", "newest first");
    }

    #[test]
    fn the_same_thing_said_again_is_one_fact_not_two() {
        let s = store("dedupe");
        let a = s.remember("My sister's name is Asha", "user", 1).unwrap();
        let b = s.remember("  my sister's NAME is asha. ", "user", 9).unwrap();
        assert_eq!((a.id, s.count()), (b.id, 1));
        assert_eq!(s.get(a.id).unwrap().last_used, 9, "it was touched");
    }

    #[test]
    fn a_secret_said_aloud_is_not_kept_and_neither_is_an_empty_or_a_huge_text() {
        let s = store("secret");
        for secret in ["my password=hunter2secret99", "my token is ghp_abcdefghijklmnopqrstuvwxyz0123456789", "the api key is sk-abcdef0123456789abcdef"] {
            assert_eq!(s.remember(secret, "user", 1).unwrap_err(), WriteError::LooksLikeASecret, "{secret}");
        }
        assert_eq!(s.remember("   ", "user", 1).unwrap_err(), WriteError::Empty);
        assert_eq!(s.remember(&"word ".repeat(100), "user", 1).unwrap_err(), WriteError::TooLong);
        assert_eq!(s.count(), 0, "nothing was stored, masked or otherwise");
        assert!(WriteError::LooksLikeASecret.to_string().contains("do not keep"));
    }

    #[test]
    fn the_store_is_full_at_two_hundred_and_says_so() {
        let s = store("full");
        for i in 0..MAX_FACTS {
            s.remember(&format!("fact number {i}"), "user", i).unwrap();
        }
        assert_eq!(s.remember("one more", "user", 999).unwrap_err(), WriteError::Full);
        assert!(s.forget(s.list(false, 1)[0].id));
        assert!(s.remember("one more", "user", 999).is_ok());
    }

    #[test]
    fn forgetting_deletes_the_row_and_a_query_forgets_only_what_all_its_words_name() {
        let s = store("forget");
        s.remember("My sister's name is Asha", "user", 1).unwrap();
        s.remember("My brother's name is Ravi", "user", 2).unwrap();
        s.remember("I prefer short answers", "user", 3).unwrap();
        assert!(s.forget_matching("my cousin").is_empty());
        assert!(s.forget_matching("the").is_empty(), "words that name nothing forget nothing");
        let gone = s.forget_matching("my sister");
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].text, "My sister's name is Asha");
        assert_eq!(s.count(), 2);
        assert!(s.list(true, 10).iter().all(|f| !f.text.contains("Asha")), "deleted, not hidden");
        assert!(!s.forget(gone[0].id), "nothing left to forget");
        assert_eq!(s.clear(), 2);
        assert_eq!(s.count(), 0);
    }

    /// Does any file of the folder (the database, a log, a journal) hold these bytes?
    fn some_file_holds(dir: &std::path::Path, needle: &str) -> bool {
        std::fs::read_dir(dir).unwrap().filter_map(Result::ok).any(|e| std::fs::read(e.path()).map(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes())).unwrap_or(false))
    }

    #[test]
    fn what_is_forgotten_is_gone_from_the_file_and_from_every_file_beside_it() {
        let dir = std::env::temp_dir().join(format!("aqua_user_facts_bytes_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = FactStore::open(dir.join("user_memory.db")).unwrap();
        let one = s.remember("My sister's name is Asha-QZXJ-ONE", "user", 1).unwrap();
        s.remember("My brother's name is Ravi-QZXJ-TWO", "user", 2).unwrap();
        s.remember("My cousin's name is Meena-QZXJ-THREE", "user", 3).unwrap();
        assert!(some_file_holds(&dir, "Asha-QZXJ-ONE"), "it was written (the test can fail)");
        assert!(s.forget(one.id));
        assert!(!some_file_holds(&dir, "Asha-QZXJ-ONE"), "forgotten by id");
        assert!(some_file_holds(&dir, "Ravi-QZXJ-TWO"), "what was not forgotten is still there");
        assert_eq!(s.forget_matching("my brother").len(), 1);
        assert!(!some_file_holds(&dir, "Ravi-QZXJ-TWO"), "forgotten by what it said");
        assert_eq!(s.clear(), 1);
        assert!(!some_file_holds(&dir, "Meena-QZXJ-THREE"), "wiped");
        assert!(!dir.join("user_memory.db-wal").exists(), "there is no write-ahead log to hold a copy");
    }

    #[test]
    fn a_database_written_in_the_old_mode_is_rewritten_once_so_that_what_was_forgotten_in_it_goes_too() {
        let dir = std::env::temp_dir().join(format!("aqua_user_facts_old_mode_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("user_memory.db");
        drop(FactStore::open(&path).unwrap());
        {
            // What the earlier build did: a write-ahead log, no secure_delete, a delete that leaves the text in its page.
            let c = Connection::open(&path).unwrap();
            let _: String = c.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0)).unwrap();
            c.execute_batch("PRAGMA secure_delete = OFF;").unwrap();
            c.execute("INSERT INTO facts (kind, text, norm, source, created) VALUES ('fact', 'Asha-QZXJ-OLD', 'asha qzxj old', 'user', 1)", []).unwrap();
            c.execute("DELETE FROM facts", []).unwrap();
        }
        assert!(some_file_holds(&dir, "Asha-QZXJ-OLD"), "the old mode left the forgotten text in the file");
        let s = FactStore::open(&path).unwrap();
        assert!(!some_file_holds(&dir, "Asha-QZXJ-OLD"), "opening rewrote it");
        assert!(!dir.join("user_memory.db-wal").exists());
        assert_eq!(s.count(), 0);
    }

    #[test]
    fn sensitive_facts_are_marked_and_left_out_unless_asked_for() {
        let s = store("sensitive");
        s.remember("I take medication for my blood pressure", "user", 1).unwrap();
        s.remember("I live in Pune", "user", 2).unwrap();
        assert_eq!(s.list(false, 10).len(), 1);
        assert_eq!(s.list(true, 10).len(), 2);
        assert!(s.list(true, 10).iter().any(|f| f.sensitive && f.text.contains("medication")));
        assert_eq!(s.export()["facts"].as_array().unwrap().len(), 2, "the person's own export has everything");
    }

    #[test]
    fn a_damaged_facts_file_is_moved_aside_and_the_rebuilt_store_starts_in_incognito() {
        let w = crate::test_support::TempWorld::new("user_facts_damaged");
        let path = w.write("user_memory.db", "this is not a database, only text. ".repeat(300));
        let s = FactStore::open(&path).unwrap();
        assert!(s.incognito(), "the old file held the person's choice and cannot be read: nothing is remembered until they turn it off");
        assert_eq!(s.remember("I live in Pune", "user", 1).unwrap_err(), WriteError::Incognito);
        s.set_incognito(false).unwrap();
        assert!(s.remember("I live in Pune", "user", 1).is_ok());
        let aside = crate::storage::sqlite_store::leftovers(&w.root);
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert!(std::fs::read_to_string(&aside[0]).unwrap().starts_with("this is not a database"), "the old file is kept");
        // Opened again, it is the same store (no second recovery).
        drop(s);
        let s = FactStore::open(&path).unwrap();
        assert_eq!(s.count(), 1);
        assert_eq!(crate::storage::sqlite_store::leftovers(&w.root).len(), 1);
    }

    #[test]
    fn a_facts_file_from_a_newer_build_is_refused_and_left_alone_and_no_copy_of_the_facts_is_ever_made() {
        let w = crate::test_support::TempWorld::new("user_facts_newer");
        let path = w.path("user_memory.db");
        {
            let s = FactStore::open(&path).unwrap();
            s.remember("I live in Pune", "user", 1).unwrap();
            s.c().execute_batch("PRAGMA user_version = 8;").unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let err = FactStore::open(&path).err().expect("refused");
        assert!(err.contains("newer than this build") && err.contains("version 8"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(std::fs::read_dir(&w.root).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().contains(".bak-v")), "a copy would keep what the person later forgets");
    }

    #[test]
    fn the_facts_file_has_a_version_and_one_made_before_versions_is_stamped_and_keeps_its_facts() {
        let w = crate::test_support::TempWorld::new("user_facts_version");
        let path = w.path("user_memory.db");
        {
            let s = FactStore::open(&path).unwrap();
            s.remember("I live in Pune", "user", 1).unwrap();
            let v: i64 = s.c().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
            assert_eq!(v, 1);
            s.c().execute_batch("PRAGMA user_version = 0;").unwrap();
        }
        let s = FactStore::open(&path).unwrap();
        assert_eq!(s.count(), 1);
        let v: i64 = s.c().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 1);
    }

    #[test]
    fn incognito_stops_writing_and_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("aqua_user_facts_incognito_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = FactStore::open(dir.join("user_memory.db")).unwrap();
        s.set_incognito(true).unwrap();
        assert_eq!(s.remember("I live in Pune", "user", 1).unwrap_err(), WriteError::Incognito);
        drop(s);
        let s = FactStore::open(dir.join("user_memory.db")).unwrap();
        assert!(s.incognito());
        s.set_incognito(false).unwrap();
        assert!(s.remember("I live in Pune", "user", 1).is_ok());
    }

    // ---- the privacy switch fails closed (QA-18) ----------------------------------------------------------------------------------------------------

    #[test]
    fn a_memory_that_was_never_switched_is_not_incognito_and_neither_is_one_switched_off() {
        let s = store("never_switched");
        assert!(!s.incognito(), "no row is the normal state: off");
        assert!(s.remember("I live in Pune", "user", 1).is_ok());
        s.set_incognito(true).unwrap();
        s.set_incognito(false).unwrap();
        assert!(!s.incognito() && s.remember("I like tea", "user", 2).is_ok());
    }

    #[test]
    fn a_setting_that_cannot_be_read_means_incognito_and_nothing_is_remembered() {
        // The table is gone (a damaged or foreign file): the read is an error, not "no row".
        let s = store("dropped_table");
        s.c().execute_batch("DROP TABLE settings").unwrap();
        assert!(s.incognito(), "unknown means do not remember");
        assert_eq!(s.remember("I live in Pune", "user", 1).unwrap_err(), WriteError::Incognito);
        assert_eq!(s.count(), 0);
        // A value this program never writes is unknown too.
        let odd = store("odd_value");
        odd.c().execute("INSERT INTO settings (key, value) VALUES ('incognito', 'maybe')", []).unwrap();
        assert!(odd.incognito());
        assert_eq!(odd.remember("I live in Pune", "user", 1).unwrap_err(), WriteError::Incognito);
        // A value of the wrong type cannot be read as the text it should be.
        let typed = store("odd_type");
        typed.c().execute("INSERT INTO settings (key, value) VALUES ('incognito', X'00')", []).unwrap();
        assert!(typed.incognito());
    }

    #[test]
    fn with_a_database_that_cannot_be_written_incognito_cannot_be_turned_off_in_silence_and_can_still_be_turned_on() {
        // On, then the file becomes read-only: turning it off fails and says so, and it stays on.
        let s = store("read_only_off");
        s.set_incognito(true).unwrap();
        s.read_only_for_test();
        let said = s.set_incognito(false).expect_err("the write fails");
        assert!(said.contains("could not be saved") && said.contains("stays on"), "{said}");
        assert!(s.incognito());
        assert_eq!(s.remember("I live in Pune", "user", 1).unwrap_err(), WriteError::Incognito);
        // Off, then read-only: asking for incognito fails to save, but this run is closed anyway, and the answer says it does not last.
        let t = store("read_only_on");
        t.read_only_for_test();
        let said = t.set_incognito(true).expect_err("the write fails");
        assert!(said.contains("until Aqua restarts"), "{said}");
        assert!(t.incognito(), "the person asked for it and was told: it holds for this run");
        assert_eq!(t.remember("I live in Pune", "user", 1).unwrap_err(), WriteError::Incognito);
    }

    #[test]
    fn a_decision_on_a_suggestion_that_cannot_be_saved_is_an_error_and_the_suggestion_stays_pending() {
        let s = store("decision_not_saved");
        let p = s.add_proposal("lesson", &[], None, "Correction: the meeting is on Friday.", "you said so", 5).unwrap().unwrap();
        s.read_only_for_test();
        let said = s.decide_proposal(p.id, false, "ui", 6).expect_err("a no that is not saved is not a no");
        assert!(said.contains("could not be saved"), "{said}");
        assert_eq!(s.proposal(p.id).unwrap().status, "pending");
    }
}

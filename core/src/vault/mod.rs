//! An offline knowledge vault: "ask my files and saved pages", with answers that point at the passage they came from. All of it local, none of it on by
//! default.
//!
//! * **Opt-in, by folder.** Nothing is read until the person adds a folder (at most 20). Files that look like secrets (by name: `.env`, keys, password
//!   stores, credentials) are never read; neither are junctions and links, files over 25 MB, or kinds this does not read.
//! * **Search is full text with ranking** (SQLite FTS5, BM25) over passages of about a thousand characters, each with where it is (file, page, slide or
//!   sheet). The quote a result gives is a stretch of the passage itself, so it can be checked against the file. There is no embedding model on this
//!   machine yet (R3-06), so a question that shares no word with the passage finds nothing: this is a keyword search and says so. Words every English
//!   question has ("what", "is", "the") are not searched for, and a question of two or more other words finds a passage only when it has **two** of them,
//!   so that "shares no word" can be said and a run is not marked private for noise (card R6-15).
//! * **What comes back is untrusted**: a document is somebody else's text, and the answer is marked private (it is the person's files).
//! * **Forgetting is real.** The database overwrites what it deletes (`secure_delete`) and is compacted, and a test looks for the bytes.
//! * **Indexing waits for a quiet moment**: between files it pauses while a person is being served.

use crate::storage::sqlite_store::{self, Migration, Opts};
use aqua_native_tools::doc;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub mod cli;
pub mod tools;
pub mod voice;

/// The schema, one step per change; never edit a step that has shipped, add one at the end (see `storage::sqlite_store`). A file made before versions existed holds these
/// tables already: the step does nothing to it and the version is stamped.
const STEPS: &[Migration] = &[Migration {
    id: 1,
    sql: "CREATE TABLE IF NOT EXISTS folders (path TEXT PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS docs (id INTEGER PRIMARY KEY, folder TEXT NOT NULL, path TEXT NOT NULL UNIQUE, kind TEXT NOT NULL, size INTEGER NOT NULL, mtime INTEGER NOT NULL, indexed_at INTEGER NOT NULL, source TEXT NOT NULL DEFAULT 'file');
             CREATE TABLE IF NOT EXISTS chunks (id INTEGER PRIMARY KEY, doc_id INTEGER NOT NULL REFERENCES docs(id) ON DELETE CASCADE, ord INTEGER NOT NULL, loc TEXT NOT NULL, text TEXT NOT NULL);
             CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(text, content='chunks', content_rowid='id', tokenize='porter unicode61');
             CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text); END;
             CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES ('delete', old.id, old.text); END;",
}];

pub const MAX_FOLDERS: usize = 20;
pub const MAX_FILE_BYTES: u64 = 25 << 20;
/// How much of a passage goes into one chunk.
const CHUNK_CHARS: usize = 1_000;
const OVERLAP_CHARS: usize = 100;
const QUOTE_CHARS: usize = 320;
const MAX_QUERY_WORDS: usize = 12;
const MAX_FILES_PER_RUN: usize = 5_000;

/// Names that are not read, whatever the folder: secrets by the look of the name.
pub fn looks_secret(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == ".env"
        || n.starts_with(".env.")
        || n.ends_with(".pem")
        || n.ends_with(".key")
        || n.ends_with(".pfx")
        || n.ends_with(".p12")
        || n.ends_with(".kdbx")
        || n.ends_with(".ppk")
        || n.starts_with("id_rsa")
        || n.starts_with("id_ed25519")
        || n.starts_with("id_ecdsa")
        || n.contains("password")
        || n.contains("secret")
        || n.contains("credential")
        || n.contains("token")
        || n == ".netrc"
        || n == ".npmrc"
}

fn kind_of(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "txt" | "text" => Some("text"),
        "md" | "markdown" => Some("markdown"),
        _ => doc::kind_of(path),
    }
}

fn db_err(e: rusqlite::Error) -> String {
    format!("vault: {e}")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Passage {
    pub path: String,
    /// Where in the file: `page 3`, `slide 2`, `sheet Sales`, or `part 1`.
    pub loc: String,
    /// A stretch of the passage, as it is in the file (cut at the ends, never changed).
    pub quote: String,
    pub score: f64,
    /// `file` or `saved page`.
    pub source: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct IndexReport {
    pub indexed: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub skipped_secret: usize,
    pub skipped_other: usize,
    pub failed: Vec<String>,
    pub paused: bool,
    /// The run stopped reading at the limit of files for one run: the files it did not get to were kept as they were and not forgotten, and the next run goes on.
    pub truncated: bool,
    /// Folders (or parts of them) that could not be listed: a drive that is not there, a share that is offline, a folder without the right to read it. What was
    /// read from them before is kept, because "could not be listed" is not "gone".
    pub unlisted: Vec<String>,
}

/// What taking a folder off came to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Removed {
    /// How many documents read from the folder were forgotten.
    pub documents: usize,
    /// Set when the file could not be compacted afterwards. The documents and the folder are gone from the vault all the same, and what was deleted is overwritten in
    /// place (`secure_delete`); the compaction is tried again the next time something is forgotten.
    pub compact_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Stats {
    pub docs: i64,
    pub chunks: i64,
    pub folders: Vec<String>,
    pub paused: bool,
}

pub struct Vault {
    /// The one that writes. `store` holds it for the whole of one file.
    conn: Mutex<Connection>,
    /// A second, read-only connection to the same file for searching and counting (card R6-15): with one connection a search waited for the file that was being
    /// stored, inside a request of the web server. In this journal mode a reader is only held up for the moment a write is committed.
    reader: Mutex<Connection>,
    #[cfg(test)]
    compact_fails: std::sync::atomic::AtomicBool,
    /// Folders the vault never reads from or into, whatever is chosen: Aqua's own data (its tokens, records, settings and databases). Without this a folder
    /// that holds the data folder, or is inside it, would hand the agent through `vault.search` what the file tools refuse it.
    protected: Mutex<Vec<String>>,
}

/// A path as a person writes it: no `\\?\` prefix, and a verbatim network path (`\\?\UNC\server\share\x`, what `canonicalize` gives for `\\server\share\x`) back as
/// `\\server\share\x`. Cutting the prefix off alone left `UNC\server\share\x`, a relative path that indexed nothing and said nothing (card R6-15).
pub fn plain_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        path.to_string()
    }
}

/// A path as a string that compares the same however it was written: no `\\?\` prefix, one kind of slash, lower case (Windows paths are not case sensitive).
fn norm(path: &Path) -> String {
    plain_path(&path.to_string_lossy()).replace('\\', "/").trim_end_matches('/').to_lowercase()
}

static VAULT: std::sync::OnceLock<std::sync::Arc<Vault>> = std::sync::OnceLock::new();
/// What the last background indexing came to, and whether one is going now.
static LAST_INDEX: Mutex<Option<IndexReport>> = Mutex::new(None);
static INDEXING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The tests that start a reading in the background share the one flag above ("a reading is going"), so they must not run side by side.
#[cfg(test)]
pub(crate) static ONE_READING_AT_A_TIME: Mutex<()> = Mutex::new(());

/// Opens the program's vault (`data/vault/vault.db`). It reads nothing until the person adds a folder.
pub fn init(data_dir: &Path) -> Result<std::sync::Arc<Vault>, String> {
    let v = std::sync::Arc::new(Vault::open(data_dir)?);
    v.protect(data_dir);
    let _ = VAULT.set(v.clone());
    Ok(v)
}

pub fn global() -> Option<&'static std::sync::Arc<Vault>> {
    VAULT.get()
}

pub fn indexing() -> bool {
    INDEXING.load(std::sync::atomic::Ordering::SeqCst)
}

pub fn last_index() -> Option<IndexReport> {
    LAST_INDEX.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Reads the folders on its own thread, between the person's requests (`idle`: nobody is being served). `false` when one is going already.
pub fn index_in_background(vault: std::sync::Arc<Vault>, idle: fn() -> bool) -> bool {
    if INDEXING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return false;
    }
    let started = std::thread::Builder::new().name("aqua-vault-index".into()).spawn(move || {
        // However the reading ends (a file that makes a parser panic too), the flag is cleared: otherwise every later request would be told that one is going.
        struct Done;
        impl Drop for Done {
            fn drop(&mut self) {
                INDEXING.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _done = Done;
        let report = vault.index(&idle);
        *LAST_INDEX.lock().unwrap_or_else(|e| e.into_inner()) = Some(report);
    });
    if started.is_err() {
        INDEXING.store(false, std::sync::atomic::Ordering::SeqCst);
        return false;
    }
    true
}

pub fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join("vault").join("vault.db")
}

/// Words that carry no question: the ones nearly every English sentence has, and the ones a request to a search starts with. They are not searched for (card R6-15):
/// with them in, "what is the" matched most passages of any text and the best of them came first.
const STOP_WORDS: &[&str] = &[
    "a", "about", "after", "all", "also", "am", "an", "and", "any", "are", "as", "at", "be", "been", "being", "but", "by", "can", "could", "did", "do", "does", "for", "from", "give", "had", "has", "have",
    "how", "if", "in", "into", "is", "it", "its", "me", "my", "no", "not", "of", "on", "or", "our", "please", "search", "she", "should", "show", "so", "than", "that", "the", "their", "them", "then",
    "there", "these", "they", "this", "to", "was", "we", "were", "what", "when", "where", "which", "who", "whom", "why", "will", "with", "would", "you", "your", "tell", "find",
];

/// A query as FTS5 words: only letters and digits, each word on its own and in quotes, so that nothing the person typed can be read as a search operator. Words that
/// every question has are left out. With one word left, a passage has to have it; with two or more, a passage has to have **two** of them (every pair is allowed, so
/// the passage that has most of them is still first), which is what lets "nothing in the vault shares a word with this" be said and keeps a run from being marked
/// private for noise. `None` when no word is left.
pub fn fts_query(text: &str) -> Option<String> {
    let mut words: Vec<String> = Vec::new();
    for w in text.split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() >= 2) {
        let w = w.to_lowercase();
        if STOP_WORDS.contains(&w.as_str()) || words.contains(&w) {
            continue;
        }
        words.push(w);
        if words.len() == MAX_QUERY_WORDS {
            break;
        }
    }
    match words.len() {
        0 => None,
        1 => Some(format!("\"{}\"", words[0])),
        _ => {
            let mut pairs = Vec::new();
            for (i, a) in words.iter().enumerate() {
                for b in &words[i + 1..] {
                    pairs.push(format!("(\"{a}\" AND \"{b}\")"));
                }
            }
            Some(pairs.join(" OR "))
        }
    }
}

/// The passages of a unit of text: whole when short, else cut at a paragraph or a sentence near the size, with a little overlap so that a sentence at a
/// cut is whole in one of the two.
pub fn chunks_of(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= CHUNK_CHARS + CHUNK_CHARS / 4 {
        let t = text.trim();
        return if t.is_empty() { Vec::new() } else { vec![t.to_string()] };
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let hard_end = (start + CHUNK_CHARS).min(chars.len());
        let mut end = hard_end;
        if hard_end < chars.len() {
            // Cut at the last paragraph or sentence end in the last quarter of the chunk.
            let floor = start + CHUNK_CHARS * 3 / 4;
            if let Some(p) = (floor..hard_end).rev().find(|&i| chars[i] == '\n' || (matches!(chars[i], '.' | '?' | '!') && chars.get(i + 1).is_some_and(|c| c.is_whitespace()))) {
                end = p + 1;
            }
        }
        let piece: String = chars[start..end].iter().collect();
        if !piece.trim().is_empty() {
            out.push(piece.trim().to_string());
        }
        if end >= chars.len() {
            break;
        }
        start = end.saturating_sub(OVERLAP_CHARS).max(start + 1);
    }
    out
}

impl Vault {
    /// Opens `data/vault/vault.db`. `secure_delete` is on and the journal is not a write-ahead log, so that what is deleted is overwritten in the file and
    /// not kept in a second file.
    pub fn open(data_dir: &Path) -> Result<Vault, String> {
        Self::open_at(&db_path(data_dir))
    }

    pub fn open_at(path: &Path) -> Result<Vault, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        // No write-ahead log and overwritten deletions (the private profile), foreign keys on, a 5 s wait for another program (`aqua-cli vault index`) that writes the same file.
        // A file that is not a database is moved aside and the index starts again: it can be built from the folders once more.
        let opened = sqlite_store::open_migrated(path, &Opts::private().foreign_keys(true), STEPS).map_err(|e| format!("vault: {e}"))?;
        let c = opened.conn;
        let reader = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(db_err)?;
        reader.busy_timeout(std::time::Duration::from_secs(5)).map_err(db_err)?;
        Ok(Vault {
            conn: Mutex::new(c),
            reader: Mutex::new(reader),
            #[cfg(test)]
            compact_fails: std::sync::atomic::AtomicBool::new(false),
            protected: Mutex::new(Vec::new()),
        })
    }

    /// From now on nothing under `folder` is read, and no folder under it is accepted.
    pub fn protect(&self, folder: &Path) {
        let canon = std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
        self.protected.lock().unwrap_or_else(|e| e.into_inner()).push(norm(&canon));
    }

    fn is_protected(&self, path: &Path) -> bool {
        let p = norm(path);
        self.protected.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|root| p == *root || p.starts_with(&format!("{root}/")))
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        let c = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        // `PRAGMA`s of a connection that was taken again.
        let _ = c.execute_batch("PRAGMA secure_delete = ON; PRAGMA foreign_keys = ON;");
        c
    }

    /// The read-only connection, for what only looks.
    fn read(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.reader.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn paused(&self) -> bool {
        self.read().query_row("SELECT value FROM meta WHERE key = 'paused'", [], |r| r.get::<_, String>(0)).optional().ok().flatten().is_some_and(|v| v == "1")
    }

    pub fn set_paused(&self, paused: bool) -> Result<(), String> {
        self.conn().execute("INSERT INTO meta (key, value) VALUES ('paused', ?1) ON CONFLICT(key) DO UPDATE SET value = ?1", [if paused { "1" } else { "0" }]).map_err(db_err)?;
        Ok(())
    }

    pub fn folders(&self) -> Vec<String> {
        let c = self.read();
        let Ok(mut st) = c.prepare("SELECT path FROM folders ORDER BY path") else { return Vec::new() };
        let rows = st.query_map([], |r| r.get::<_, String>(0)).map(|rows| rows.filter_map(Result::ok).collect()).unwrap_or_default();
        rows
    }

    /// Adds a folder to read. It has to exist, be a folder, and not be a whole drive.
    pub fn add_folder(&self, folder: &Path) -> Result<String, String> {
        let canon = std::fs::canonicalize(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
        if !canon.is_dir() {
            return Err(format!("{} is not a folder", canon.display()));
        }
        if canon.parent().is_none() || canon.components().count() <= 2 {
            return Err("a whole drive is too much: choose a folder inside it".into());
        }
        if self.is_protected(&canon) {
            return Err("that folder is Aqua's own data (its tokens, records and settings): it is not for the vault".into());
        }
        let shown = plain_path(&canon.to_string_lossy());
        if self.folders().len() >= MAX_FOLDERS {
            return Err(format!("at most {MAX_FOLDERS} folders"));
        }
        self.conn().execute("INSERT OR IGNORE INTO folders (path) VALUES (?1)", [&shown]).map_err(db_err)?;
        Ok(shown)
    }

    /// Takes a folder off and forgets everything read from it. The documents and the folder are deleted first and the file is compacted after: a compaction that fails
    /// is reported in the result and does not leave the folder on the list (it used to, so that the next reading read what had just been forgotten).
    pub fn remove_folder(&self, folder: &str) -> Result<Removed, String> {
        let documents = self.conn().execute("DELETE FROM docs WHERE folder = ?1", [folder]).map_err(db_err)?;
        self.conn().execute("DELETE FROM folders WHERE path = ?1", [folder]).map_err(db_err)?;
        let compact_error = self.compact().err();
        Ok(Removed { documents, compact_error })
    }

    /// Reads the folders: what is new or changed is read again, what is gone is forgotten. `idle` is asked between files and the run waits while it says
    /// no (a person is being served); `paused` stops the run at once.
    pub fn index(&self, idle: &dyn Fn() -> bool) -> IndexReport {
        self.index_up_to(idle, MAX_FILES_PER_RUN)
    }

    /// [`Vault::index`] with the limit of files for one run given. What is no longer in the folders is forgotten only when it was listed and is not there: a file the run
    /// did not get to because of the limit, and everything under a folder that could not be listed, is kept.
    fn index_up_to(&self, idle: &dyn Fn() -> bool, max_files: usize) -> IndexReport {
        let mut report = IndexReport::default();
        if self.paused() {
            report.paused = true;
            return report;
        }
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut unlisted_dirs: Vec<String> = Vec::new();
        let mut files = 0usize;
        for folder in self.folders() {
            let mut stack = vec![PathBuf::from(&folder)];
            while let Some(dir) = stack.pop() {
                let rd = match std::fs::read_dir(&dir) {
                    Ok(rd) => rd,
                    Err(e) => {
                        report.unlisted.push(format!("{}: {e}", dir.display()));
                        unlisted_dirs.push(norm(&dir));
                        continue;
                    }
                };
                for entry in rd {
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(e) => {
                            // A listing that broke off in the middle: what is below this folder is not known to be gone.
                            report.unlisted.push(format!("{}: {e}", dir.display()));
                            unlisted_dirs.push(norm(&dir));
                            break;
                        }
                    };
                    let path = entry.path();
                    let name = entry.file_name().to_string_lossy().into_owned();
                    // Links and junctions are never followed: they can lead anywhere.
                    let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
                    if meta.file_type().is_symlink() || is_reparse_point(&meta) {
                        continue;
                    }
                    if self.is_protected(&path) {
                        continue;
                    }
                    if meta.is_dir() {
                        if !name.starts_with('.') && !looks_secret(&name) {
                            stack.push(path);
                        }
                        continue;
                    }
                    if looks_secret(&name) {
                        report.skipped_secret += 1;
                        continue;
                    }
                    let Some(kind) = kind_of(&path) else {
                        report.skipped_other += 1;
                        continue;
                    };
                    let shown = path.to_string_lossy().into_owned();
                    if meta.len() > MAX_FILE_BYTES {
                        report.skipped_other += 1;
                        continue;
                    }
                    if files >= max_files {
                        // Not read this time and not forgotten either: it is there.
                        report.truncated = true;
                        seen.insert(shown);
                        continue;
                    }
                    files += 1;
                    seen.insert(shown.clone());
                    while !idle() {
                        if self.paused() {
                            report.paused = true;
                            return report;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0);
                    let known: Option<(i64, i64)> = self.read().query_row("SELECT size, mtime FROM docs WHERE path = ?1", [&shown], |r| Ok((r.get(0)?, r.get(1)?))).optional().ok().flatten();
                    if known == Some((meta.len() as i64, mtime)) {
                        report.unchanged += 1;
                        continue;
                    }
                    match read_units(&path, kind) {
                        Ok(units) => match self.store(&folder, &shown, kind, meta.len() as i64, mtime, "file", &units) {
                            Ok(()) => report.indexed += 1,
                            Err(e) => report.failed.push(format!("{shown}: {e}")),
                        },
                        Err(e) => report.failed.push(format!("{shown}: {e}")),
                    }
                }
            }
        }
        // What was read before and is no longer in the folders: listed, and not there. A path under a folder that could not be listed is not known to be gone.
        let c = self.read();
        let gone: Vec<String> = c
            .prepare("SELECT path FROM docs WHERE source = 'file'")
            .and_then(|mut st| st.query_map([], |r| r.get::<_, String>(0)).map(|rows| rows.filter_map(Result::ok).collect::<Vec<String>>()))
            .unwrap_or_default()
            .into_iter()
            .filter(|p| !seen.contains(p))
            .filter(|p| {
                let n = norm(Path::new(p));
                !unlisted_dirs.iter().any(|d| n == *d || n.starts_with(&format!("{d}/")))
            })
            .collect();
        drop(c);
        for p in gone {
            if self.forget_path(&p).is_ok() {
                report.removed += 1;
            }
        }
        report
    }

    fn store(&self, folder: &str, path: &str, kind: &str, size: i64, mtime: i64, source: &str, units: &[(String, String)]) -> Result<(), String> {
        let mut c = self.conn();
        let tx = c.transaction().map_err(db_err)?;
        tx.execute("DELETE FROM docs WHERE path = ?1", [path]).map_err(db_err)?;
        tx.execute("INSERT INTO docs (folder, path, kind, size, mtime, indexed_at, source) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![folder, path, kind, size, mtime, now_ms(), source]).map_err(db_err)?;
        let doc_id = tx.last_insert_rowid();
        let mut ord = 0;
        for (label, text) in units {
            for (part, chunk) in chunks_of(text).into_iter().enumerate() {
                let loc = if label.is_empty() { format!("part {}", ord + 1) } else if part == 0 { label.clone() } else { format!("{label}, part {}", part + 1) };
                tx.execute("INSERT INTO chunks (doc_id, ord, loc, text) VALUES (?1, ?2, ?3, ?4)", params![doc_id, ord, loc, chunk]).map_err(db_err)?;
                ord += 1;
            }
        }
        tx.commit().map_err(db_err)
    }

    /// Saves a page or a note the person (or a research run) wants kept: text with a title and where it came from. It is searched like a file.
    pub fn add_text(&self, title: &str, source_note: &str, text: &str) -> Result<(), String> {
        let title = title.trim();
        if title.is_empty() || title.chars().count() > 200 || text.trim().is_empty() || text.chars().count() > 200_000 {
            return Err("a title of 1 to 200 characters and text of at most 200,000".into());
        }
        let path = format!("saved page: {title}");
        let body = if source_note.trim().is_empty() { text.to_string() } else { format!("{}\n{text}", source_note.trim()) };
        self.store("(saved pages)", &path, "page", text.len() as i64, now_ms(), "page", &[(String::new(), body)])
    }

    /// The passages that fit `query`, best first. A stretch of each is returned as it is in the file.
    pub fn search(&self, query: &str, limit: usize) -> Vec<Passage> {
        let Some(q) = fts_query(query) else { return Vec::new() };
        let c = self.read();
        let Ok(mut st) = c.prepare(
            "SELECT d.path, k.loc, snippet(chunks_fts, 0, '', '', '…', 40), bm25(chunks_fts), d.source, k.text
             FROM chunks_fts JOIN chunks k ON k.id = chunks_fts.rowid JOIN docs d ON d.id = k.doc_id
             WHERE chunks_fts MATCH ?1 ORDER BY bm25(chunks_fts) LIMIT ?2",
        ) else {
            return Vec::new();
        };
        let rows = st
            .query_map(params![q, limit.clamp(1, 20) as i64], |r| {
                let (path, loc, snip, score, source, full): (String, String, String, f64, String, String) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get::<_, String>(4)?, r.get(5)?);
                // The snippet is a stretch of the passage with an ellipsis where it was cut; the quote is that stretch without the ellipses.
                let core = snip.trim_matches('…').trim().to_string();
                let quote = if full.contains(&core) { core } else { full.chars().take(QUOTE_CHARS).collect() };
                Ok(Passage { path, loc, quote: quote.chars().take(QUOTE_CHARS).collect(), score, source: if source == "page" { "saved page".into() } else { "file".into() } })
            })
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default();
        rows
    }

    fn forget_path(&self, path: &str) -> Result<(), String> {
        self.conn().execute("DELETE FROM docs WHERE path = ?1", [path]).map_err(db_err)?;
        Ok(())
    }

    /// Forgets everything read from one folder, and compacts the file so that nothing of it is left in it.
    pub fn forget_folder(&self, folder: &str) -> Result<usize, String> {
        let n = self.conn().execute("DELETE FROM docs WHERE folder = ?1", [folder]).map_err(db_err)?;
        self.compact()?;
        Ok(n)
    }

    /// Forgets everything: files, saved pages and the list of folders.
    pub fn forget_all(&self) -> Result<usize, String> {
        let n = self.conn().execute("DELETE FROM docs", []).map_err(db_err)?;
        self.conn().execute("DELETE FROM folders", []).map_err(db_err)?;
        self.compact()?;
        Ok(n)
    }

    fn compact(&self) -> Result<(), String> {
        #[cfg(test)]
        if self.compact_fails.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("vault: simulated failure of VACUUM".into());
        }
        let c = self.conn();
        c.execute("INSERT INTO chunks_fts(chunks_fts) VALUES ('optimize')", []).ok();
        c.execute_batch("VACUUM").map_err(db_err)
    }

    pub fn stats(&self) -> Stats {
        let c = self.read();
        let count = |sql: &str| c.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0);
        let (docs, chunks) = (count("SELECT COUNT(*) FROM docs"), count("SELECT COUNT(*) FROM chunks"));
        drop(c);
        Stats { docs, chunks, folders: self.folders(), paused: self.paused() }
    }
}

#[cfg(windows)]
fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &std::fs::Metadata) -> bool {
    false
}

/// The text of a file by parts: (where, text).
fn read_units(path: &Path, kind: &str) -> Result<Vec<(String, String)>, String> {
    match kind {
        "text" | "markdown" => {
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            let text = String::from_utf8_lossy(&bytes).replace('\u{0}', " ");
            Ok(vec![(String::new(), text)])
        }
        _ => {
            let document = doc::extract(path).map_err(|e| e.message)?;
            Ok(document.units.into_iter().map(|u| (u.label, u.text)).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_vault_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn vault_in(base: &Path) -> Vault {
        Vault::open_at(&base.join("vault.db")).unwrap()
    }

    fn always() -> bool {
        true
    }

    #[test]
    fn the_vault_file_has_a_version_and_a_damaged_one_is_moved_aside_and_the_index_starts_again() {
        let base = folder("file");
        {
            let v = vault_in(&base);
            let docs = base.join("docs");
            std::fs::create_dir_all(&docs).unwrap();
            v.add_folder(&docs).unwrap();
            assert_eq!(v.folders().len(), 1);
        }
        let version: i64 = Connection::open(base.join("vault.db")).unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 1);
        // No log file, and the journal is not a write-ahead log.
        let mode: String = Connection::open(base.join("vault.db")).unwrap().query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode.to_lowercase(), "delete");
        std::fs::write(base.join("vault.db"), "this is not a database, only text. ".repeat(300)).unwrap();
        let v = vault_in(&base);
        assert!(v.folders().is_empty(), "the index starts again; the folders are the person's to add back");
        let aside = crate::storage::sqlite_store::leftovers(&base);
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert!(std::fs::read_to_string(&aside[0]).unwrap().starts_with("this is not a database"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_vault_file_from_a_newer_build_is_refused_and_left_alone() {
        let base = folder("newer");
        drop(vault_in(&base));
        {
            let c = Connection::open(base.join("vault.db")).unwrap();
            c.execute_batch("PRAGMA user_version = 6;").unwrap();
        }
        let before = std::fs::read(base.join("vault.db")).unwrap();
        let err = Vault::open_at(&base.join("vault.db")).err().expect("refused");
        assert!(err.contains("newer than this build") && err.contains("version 6"), "{err}");
        assert_eq!(std::fs::read(base.join("vault.db")).unwrap(), before);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_query_is_only_words_so_nothing_typed_is_a_search_operator() {
        // Words every question has are left out, and two words of the rest are needed (every pair is allowed).
        assert_eq!(fts_query("What is the Launch code?").as_deref(), Some("(\"launch\" AND \"code\")"));
        // What was typed is words in quotes: `OR` and `NEAR` here are two words, not operators (the operators in the query are the ones this function makes).
        assert_eq!(fts_query("foo\" OR NEAR(a b) -x *").as_deref(), Some("(\"foo\" AND \"near\")"));
        assert!(fts_query("  ? ! a ").is_none());
        assert!(fts_query(&"word ".repeat(40)).unwrap().matches(" OR ").count() < MAX_QUERY_WORDS);
    }

    #[test]
    fn long_text_is_cut_into_passages_at_sentence_ends_with_a_little_overlap_and_every_word_is_in_some_passage() {
        let text: String = (0..200).map(|i| format!("Sentence number {i} says something about topic{i}.")).collect::<Vec<_>>().join(" ");
        let chunks = chunks_of(&text);
        assert!(chunks.len() >= 8 && chunks.iter().all(|c| c.chars().count() <= CHUNK_CHARS + 4), "{} chunks", chunks.len());
        for i in 0..200 {
            assert!(chunks.iter().any(|c| c.contains(&format!("topic{i}."))), "topic{i} is in no passage");
        }
        assert!(chunks[0].ends_with('.'), "cut at a sentence end: {}", &chunks[0][chunks[0].len().saturating_sub(40)..]);
        assert_eq!(chunks_of("short text"), vec!["short text".to_string()]);
        assert!(chunks_of("   ").is_empty());
    }

    #[test]
    fn nothing_is_read_until_a_folder_is_added_and_then_only_files_of_the_kinds_it_reads_that_do_not_look_like_secrets() {
        let base = folder("optin");
        let docs = base.join("docs");
        std::fs::create_dir_all(docs.join("sub")).unwrap();
        std::fs::write(docs.join("notes.txt"), "The launch code word is PINEAPPLE-42. Keep it quiet.").unwrap();
        std::fs::write(docs.join("sub").join("plan.md"), "# Plan\nWe ship the garden project in March.").unwrap();
        std::fs::write(docs.join("table.csv"), "name,score\nanna,17\nbo,23\n").unwrap();
        std::fs::write(docs.join(".env"), "API_KEY=sk-should-never-be-read-123456").unwrap();
        std::fs::write(docs.join("id_rsa"), "-----BEGIN PRIVATE KEY-----").unwrap();
        std::fs::write(docs.join("passwords.txt"), "bank: hunter2").unwrap();
        std::fs::write(docs.join("photo.png"), [0u8; 10]).unwrap();
        let v = vault_in(&base);
        assert!(v.search("launch", 5).is_empty(), "nothing is read before a folder is added");
        assert!(v.index(&always).indexed == 0);
        v.add_folder(&docs).unwrap();
        let r = v.index(&always);
        assert_eq!((r.indexed, r.skipped_secret, r.skipped_other), (3, 3, 1), "{r:?}");
        let found = v.search("launch code word", 5);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].path.ends_with("notes.txt") && found[0].quote.contains("PINEAPPLE-42"), "{found:?}");
        assert!(v.search("hunter2 API_KEY BEGIN PRIVATE", 5).is_empty(), "the secrets were never read");
        assert!(v.search("garden project", 5)[0].path.ends_with("plan.md"));
        assert!(v.search("anna", 5)[0].path.ends_with("table.csv"));
        assert!(v.add_folder(&base.join("missing")).is_err());
        assert!(v.add_folder(Path::new("C:\\")).is_err() || cfg!(not(windows)), "a whole drive is refused");
    }

    /// Card R6-15: every English question matched most passages because words of two letters or more were OR-ed with no stop list, so "shares no word" almost never
    /// showed and a run was marked private for noise.
    #[test]
    fn a_question_of_common_words_finds_nothing_and_a_passage_needs_two_of_the_words_asked_for() {
        let base = folder("stop");
        let docs = base.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("invoice.txt"), "The invoice from Acme is due on Friday and the park is closed.").unwrap();
        std::fs::write(docs.join("weather.txt"), "The weather is nice and the park is open all week.").unwrap();
        std::fs::write(docs.join("anvils.txt"), "Acme makes anvils.").unwrap();
        let v = vault_in(&base);
        v.add_folder(&docs).unwrap();
        v.index(&always);
        let names = |q: &str| {
            let mut n: Vec<String> = v.search(q, 10).into_iter().map(|p| Path::new(&p.path).file_name().unwrap().to_string_lossy().into_owned()).collect();
            n.sort();
            n
        };
        // Only words every question has: nothing to look for, and nothing found (these matched every passage that had "the" or "is").
        for common in ["what is the", "who is it", "can you tell me about the", "is it the"] {
            assert!(fts_query(common).is_none(), "{common}");
            assert!(names(common).is_empty(), "{common}");
        }
        // Two words that are asked for: a passage has to have both.
        assert_eq!(names("when is the invoice due"), vec!["invoice.txt"]);
        assert_eq!(names("the park is open"), vec!["weather.txt"]);
        // Two words that no one passage has together: nothing, and it says so (`acme` and `weather` are each in a passage, never in one).
        assert!(names("acme weather").is_empty());
        // One word asked for: a passage has to have it.
        assert_eq!(names("Acme"), vec!["anvils.txt", "invoice.txt"]);
        // Three words: any two of them are enough, and the passage with more is first.
        assert_eq!(names("acme invoice friday").first().map(String::as_str), Some("invoice.txt"));
        assert_eq!(names("acme friday park").len(), 1, "any two of three: the invoice has all three, the others have one or two of them ({:?})", names("acme friday park"));
    }

    /// Card R6-15: over 5,000 files the cap was silent, and the files after the first 5,000 were deleted from the index, because only the files read were "seen".
    #[test]
    fn a_run_that_stops_at_the_limit_says_so_and_does_not_forget_the_files_it_did_not_get_to() {
        let base = folder("cap");
        let docs = base.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        for i in 0..4 {
            std::fs::write(docs.join(format!("f{i}.txt")), format!("passage number {i} about zebras and kiwis")).unwrap();
        }
        let v = vault_in(&base);
        v.add_folder(&docs).unwrap();
        let first = v.index_up_to(&always, 100);
        assert_eq!((first.indexed, first.truncated, first.removed), (4, false, 0), "{first:?}");
        // A run that may read two files: it says it stopped, and the other two are not forgotten.
        std::fs::write(docs.join("f0.txt"), "passage number 0 about zebras and kiwis, edited").unwrap();
        let capped = v.index_up_to(&always, 2);
        assert!(capped.truncated && capped.removed == 0, "{capped:?}");
        assert_eq!(v.search("zebras kiwis", 10).len(), 4, "every file is still in the index");
        // A run with room goes on from there.
        let rest = v.index_up_to(&always, 100);
        assert!(!rest.truncated && rest.removed == 0 && v.stats().docs == 4, "{rest:?}");
    }

    /// Card R6-15: a folder that could not be listed (a drive that is not there, a share that is offline) counted as "gone", and everything read from it was forgotten.
    #[test]
    fn a_folder_that_cannot_be_listed_keeps_what_was_read_from_it_and_is_reported() {
        let base = folder("unlisted");
        let docs = base.join("docs");
        let moved = base.join("docs_away");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), "the quarterly budget for the garden project").unwrap();
        std::fs::write(docs.join("b.txt"), "the garden project needs more soil").unwrap();
        let v = vault_in(&base);
        v.add_folder(&docs).unwrap();
        assert_eq!(v.index(&always).indexed, 2);
        // The folder goes away (an unplugged drive does that): it cannot be listed.
        std::fs::rename(&docs, &moved).unwrap();
        let r = v.index(&always);
        assert_eq!(r.removed, 0, "{r:?}");
        assert_eq!(r.unlisted.len(), 1, "the folder is reported: {r:?}");
        assert_eq!(v.search("garden project", 5).len(), 2, "what was read is kept");
        // It comes back: nothing is unlisted any more, and nothing was lost.
        std::fs::rename(&moved, &docs).unwrap();
        let again = v.index(&always);
        assert!(again.unlisted.is_empty() && again.unchanged == 2 && again.removed == 0, "{again:?}");
        // A file that is really gone from a folder that can be listed is still forgotten.
        std::fs::remove_file(docs.join("b.txt")).unwrap();
        assert_eq!(v.index(&always).removed, 1);
        assert_eq!(v.search("garden project", 5).len(), 1);
    }

    /// Card R6-15: `\\nas\docs` canonicalises to `\\?\UNC\nas\docs`; cutting `\\?\` off left `UNC\nas\docs`, a relative path that indexed nothing and said nothing.
    #[test]
    fn a_network_path_keeps_its_two_slashes_and_every_spelling_of_it_compares_the_same() {
        assert_eq!(plain_path(r"\\?\UNC\nas\docs\reports"), r"\\nas\docs\reports");
        assert_eq!(plain_path(r"\\?\C:\Users\me\Documents"), r"C:\Users\me\Documents");
        assert_eq!(plain_path(r"\\nas\docs"), r"\\nas\docs", "a path that is not verbatim is as it was");
        assert_eq!(plain_path(r"C:\plain"), r"C:\plain");
        let verbatim = norm(Path::new(r"\\?\UNC\NAS\Docs\Reports\"));
        assert_eq!(verbatim, "//nas/docs/reports");
        assert_eq!(verbatim, norm(Path::new(r"\\nas\docs\reports")), "the same share however it was written");
        assert_ne!(norm(Path::new(r"\\?\UNC\nas\docs")), norm(Path::new(r"UNC\nas\docs")), "and not a relative folder that happens to be called UNC");
    }

    /// Card R6-15: a failed `VACUUM` returned before the folder row was deleted, so the next reading read the forgotten folder again.
    #[test]
    fn a_compaction_that_fails_still_takes_the_folder_off_and_says_so() {
        let base = folder("vacuum");
        let docs = base.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), "the secret plan is a garden").unwrap();
        let v = vault_in(&base);
        let shown = v.add_folder(&docs).unwrap();
        assert_eq!(v.index(&always).indexed, 1);
        v.compact_fails.store(true, std::sync::atomic::Ordering::SeqCst);
        let removed = v.remove_folder(&shown).unwrap();
        assert_eq!(removed.documents, 1);
        assert!(removed.compact_error.as_deref().is_some_and(|e| e.contains("VACUUM")), "{removed:?}");
        assert!(v.folders().is_empty(), "the folder is off the list");
        assert!(v.search("secret plan garden", 5).is_empty() && v.stats().docs == 0);
        assert_eq!(v.index(&always).indexed, 0, "the next reading does not read it again");
        // Without the failure the report says nothing.
        v.compact_fails.store(false, std::sync::atomic::Ordering::SeqCst);
        let shown = v.add_folder(&docs).unwrap();
        v.index(&always);
        assert_eq!(v.remove_folder(&shown).unwrap(), Removed { documents: 1, compact_error: None });
    }

    /// Card R6-15: search and the counts waited on the one connection lock while `store` held it for a whole file, inside the web server's request.
    #[test]
    fn a_search_during_indexing_does_not_wait_for_the_file_that_is_being_stored() {
        let base = folder("lock");
        let docs = base.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), "the garden project needs soil").unwrap();
        let v = std::sync::Arc::new(vault_in(&base));
        v.add_folder(&docs).unwrap();
        v.index(&always);
        let busy = v.clone();
        let holder = std::thread::spawn(move || {
            // What `store` does: the writing connection is held (here for 700 ms) while a file is stored.
            let _writing = busy.conn();
            std::thread::sleep(std::time::Duration::from_millis(700));
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        let started = std::time::Instant::now();
        let found = v.search("garden project", 5);
        let stats = v.stats();
        let took = started.elapsed();
        assert_eq!(found.len(), 1);
        assert_eq!(stats.docs, 1);
        assert!(took < std::time::Duration::from_millis(200), "a search and the counts took {took:?} while a file was being stored");
        holder.join().unwrap();
    }

    #[test]
    fn a_reading_that_panics_does_not_leave_the_vault_saying_that_it_is_still_indexing() {
        let _one_at_a_time = ONE_READING_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let base = folder("panics");
        std::fs::write(base.join("a.txt"), "some words to read").unwrap();
        let v = std::sync::Arc::new(vault_in(&base));
        v.add_folder(&base).unwrap();
        fn gives_up() -> bool {
            panic!("a parser gave up on a file");
        }
        assert!(index_in_background(v.clone(), gives_up));
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while indexing() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!indexing(), "the flag was left set, so every later request would be told that one is going");
        assert!(index_in_background(v, always), "and a new reading can start");
    }

    #[test]
    fn aquas_own_data_is_never_read_even_when_a_folder_that_holds_it_is_chosen_and_it_cannot_be_chosen_itself() {
        let base = folder("protected");
        let project = base.join("project");
        let data = project.join("data");
        std::fs::create_dir_all(data.join("tools")).unwrap();
        std::fs::write(project.join("readme.txt"), "The project readme says: PUBLIC-NOTE-1").unwrap();
        std::fs::write(data.join("notes.txt"), "Aqua's own notes: PRIVATE-DATA-9").unwrap();
        std::fs::write(data.join("tools").join("run.md"), "tokens PRIVATE-DATA-9").unwrap();
        let v = vault_in(&base);
        v.protect(&data);
        assert!(v.add_folder(&data).unwrap_err().contains("Aqua's own data"), "the data folder cannot be chosen");
        assert!(v.add_folder(&data.join("tools")).is_err(), "nor a folder inside it");
        // The folder that holds it can be chosen (the project's own files are what the person wants), and the data under it is left alone.
        v.add_folder(&project).unwrap();
        let r = v.index(&always);
        assert_eq!(r.indexed, 1, "{r:?}");
        assert!(v.search("PUBLIC-NOTE-1", 3).len() == 1 && v.search("PRIVATE-DATA-9", 3).is_empty());
        // Spelled differently (upper case, forward slashes), it is still the same folder.
        let spelled = PathBuf::from(data.to_string_lossy().to_uppercase().replace('\\', "/"));
        assert!(v.add_folder(&spelled).is_err());
    }

    #[test]
    fn indexing_again_reads_only_what_changed_and_forgets_what_is_gone() {
        let base = folder("incremental");
        let docs = base.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), "alpha keyword here").unwrap();
        std::fs::write(docs.join("b.txt"), "bravo keyword there").unwrap();
        let v = vault_in(&base);
        v.add_folder(&docs).unwrap();
        assert_eq!(v.index(&always).indexed, 2);
        let again = v.index(&always);
        assert_eq!((again.indexed, again.unchanged, again.removed), (0, 2, 0));
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(docs.join("a.txt"), "alpha changed zebra text").unwrap();
        std::fs::remove_file(docs.join("b.txt")).unwrap();
        let r = v.index(&always);
        assert_eq!((r.indexed, r.removed), (1, 1), "{r:?}");
        assert!(v.search("zebra", 5).len() == 1 && v.search("bravo", 5).is_empty() && v.search("keyword", 5).is_empty(), "the old text of a changed file is gone too");
        assert_eq!(v.stats().docs, 1);
    }

    #[test]
    fn results_are_ranked_and_each_quote_is_a_stretch_of_the_passage_that_can_be_found_in_the_file() {
        let base = folder("rank");
        let docs = base.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        let long: String = (0..60).map(|i| format!("Filler sentence {i} about nothing in particular.")).collect::<Vec<_>>().join(" ") + " The invoice total for March was 4,200 euros, paid by wire.";
        std::fs::write(docs.join("long.txt"), &long).unwrap();
        std::fs::write(docs.join("short.txt"), "Invoice invoice invoice total: a note about the invoice.").unwrap();
        let v = vault_in(&base);
        v.add_folder(&docs).unwrap();
        v.index(&always);
        let hits = v.search("invoice total March euros", 5);
        assert_eq!(hits.len(), 2);
        assert!(hits[0].score <= hits[1].score, "best first (bm25: lower is better)");
        for h in &hits {
            let file = std::fs::read_to_string(&h.path).unwrap();
            assert!(file.contains(&h.quote), "the quote is not in the file: {:?}", h.quote);
            assert!(h.quote.chars().count() <= QUOTE_CHARS);
        }
        let total = hits.iter().find(|h| h.path.ends_with("long.txt")).unwrap();
        assert!(total.quote.contains("4,200") || total.quote.contains("invoice total"), "{}", total.quote);
        assert!(total.loc.starts_with("part"), "{}", total.loc);
    }

    #[test]
    fn a_page_can_be_saved_and_is_found_and_forgetting_a_folder_or_everything_leaves_no_trace_in_the_file() {
        let base = folder("forget");
        let docs = base.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("diary.txt"), "My secret marker is ZXQV-FORGET-ME-7731 and nobody knows.").unwrap();
        let v = vault_in(&base);
        v.add_folder(&docs).unwrap();
        v.add_text("Rust release", "https://blog.rust-lang.org/x", "Rust 1.90.0 made lld the default linker, marker QWPL-PAGE-9912.").unwrap();
        v.index(&always);
        assert_eq!(v.search("lld linker", 5)[0].source, "saved page");
        assert_eq!(v.search("nobody knows", 5)[0].source, "file");
        let db = base.join("vault.db");
        let has = |needle: &str| String::from_utf8_lossy(&std::fs::read(&db).unwrap()).contains(needle);
        assert!(has("ZXQV-FORGET-ME-7731") && has("QWPL-PAGE-9912"));
        let folder_name = v.folders()[0].clone();
        assert_eq!(v.remove_folder(&folder_name).unwrap().documents, 1);
        assert!(!has("ZXQV-FORGET-ME-7731"), "the text of a forgotten folder is not left in the file");
        assert!(has("QWPL-PAGE-9912"), "a saved page is not part of a folder");
        assert!(v.search("nobody knows", 5).is_empty() && !v.search("lld", 5).is_empty());
        v.forget_all().unwrap();
        assert!(!has("QWPL-PAGE-9912") && v.stats().docs == 0 && v.folders().is_empty());
        assert!(v.add_text("", "", "x").is_err() && v.add_text("t", "", "  ").is_err());
    }

    #[test]
    fn indexing_can_be_paused_and_waits_while_a_person_is_being_served() {
        let base = folder("pause");
        let docs = base.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), "alpha").unwrap();
        let v = vault_in(&base);
        v.add_folder(&docs).unwrap();
        v.set_paused(true).unwrap();
        let r = v.index(&always);
        assert!(r.paused && r.indexed == 0 && v.search("alpha", 3).is_empty());
        v.set_paused(false).unwrap();
        // Busy for the first three asks, then free: it waits and goes on.
        let asked = std::cell::Cell::new(0);
        let busy_then_free = || {
            asked.set(asked.get() + 1);
            asked.get() > 3
        };
        let r = v.index(&busy_then_free);
        assert_eq!(r.indexed, 1);
        assert!(asked.get() > 3);
        // Paused while waiting: it stops.
        let v2 = vault_in(&folder("pause2"));
        std::fs::write(docs.join("b.txt"), "bravo").unwrap();
        v2.add_folder(&docs).unwrap();
        let pause_now = || {
            v2.set_paused(true).unwrap();
            false
        };
        assert!(v2.index(&pause_now).paused);
    }
}

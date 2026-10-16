//! One way to open and upgrade Aqua's SQLite files (card OA3-02; tasks AQ-09, CX-10, EX-16 and QA-17 of the OpenCode audit).
//!
//! Before this module each store opened its database its own way: different busy timeouts, three with a schema version and eleven without, a migration
//! that ran in autocommit (a crash between an `ALTER TABLE` and the version bump left a column added and the old version, so the next start failed on
//! "duplicate column name"), a database from a newer build opened and "migrated" by an older one, and no answer at all for a file that had been damaged.
//!
//! * [`open`] makes the folder, opens the file, runs `PRAGMA quick_check(1)` (read only, and only for a file up to 32 MB), and sets the journal mode and the per-connection pragmas the
//!   store asks for ([`Opts`]). A file that is not a database or fails the check is **moved aside** as `<name>.corrupt-<unix>` (with its `-wal` and `-shm`),
//!   a fresh one is made, and [`Opened::recovered_from`] says where the old one went. Nothing is deleted: the old file keeps its data and its permissions, and
//!   the doctor lists it ([`leftovers`]). A database that is merely *locked* is an error, never a quarantine.
//! * [`migrate`] applies [`Migration`] steps in order, each in its own `BEGIN IMMEDIATE` transaction that also sets `PRAGMA user_version`, so a failing
//!   statement rolls the step **and** the version back. A database whose version is higher than the newest step is refused ([`StoreError::TooNew`]) before
//!   anything is written (an older build never rewrites a newer build's file). Before the first step that upgrades a database that already holds a version, the
//!   file is copied to `<name>.bak-v<old>` with `VACUUM INTO` (the two newest are kept; none above 500 MB), and a backup that cannot be made stops the
//!   migration with the old database untouched.
//! * [`connect`] is the light open for a store that opens a connection for every call: the pragmas that belong to a connection, nothing else.
//!
//! A step is never edited after it has shipped; a change to the shape is a new step at the end. A database made before this module existed has no version:
//! its first step is the `CREATE TABLE IF NOT EXISTS ...` statements it was already made with, which do nothing to it, and the version is stamped. A store whose
//! old builds also altered tables without a version passes an `adopt` function that looks at the columns (`migrate_adopting`).

use rusqlite::{Connection, ErrorCode, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A database file bigger than this is not copied before a migration (the copy would be as big again).
pub const BACKUP_MAX_BYTES: u64 = 500 * 1024 * 1024;
/// A database bigger than this is not scanned by `PRAGMA quick_check` when it is opened (see [`Opts::quick_check_max_bytes`]).
pub const QUICK_CHECK_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// How many `.bak-v*` copies of one database are kept.
pub const BACKUPS_KEPT: usize = 2;

/// The journal mode a store wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Journal {
    /// Write-ahead log: readers do not block the writer.
    Wal,
    /// Rollback journal, no second copy of the pages in a `-wal` file. For the stores that forget on request (facts, vault): a deleted row must be gone from the file.
    /// A database that was in WAL mode is rewritten once (`VACUUM`) when it is switched.
    Delete,
    /// Leave the mode as the file has it.
    Keep,
}

/// What a store asks of its connection.
#[derive(Debug, Clone)]
pub struct Opts {
    /// How long a locked database is waited for before the call fails (never a quarantine).
    pub busy_ms: u32,
    pub journal: Journal,
    /// Overwrite deleted text on disk. A property of the connection, so every connection sets it.
    pub secure_delete: bool,
    pub foreign_keys: bool,
    /// `PRAGMA synchronous = NORMAL` (safe with a WAL; the default is FULL).
    pub synchronous_normal: bool,
    /// Run `PRAGMA quick_check(1)` when the file is opened by [`open`].
    pub quick_check: bool,
    /// A file bigger than this is not scanned at every start (a full scan would cost more than the damage it finds); the cheap read of the schema still runs and still finds a
    /// file that is not a database at all. The file index (30 MB and growing) is the reason.
    pub quick_check_max_bytes: u64,
    /// Keep a copy (`.bak-v<n>`) before a step upgrades a database that holds a version. Off for the stores that forget on request: a copy would keep what the person asked
    /// to be forgotten.
    pub backup: bool,
    /// Move a damaged file aside and start a new one (the default). A store whose file holds a safety setting that must not silently reset (the voice lock) turns it off:
    /// the damage is then an error, as it always was, and the file is left where it is.
    pub recover: bool,
}

impl Opts {
    /// A write-ahead log, a 5 s wait for a lock, the check on.
    pub fn standard() -> Self {
        Self { busy_ms: 5_000, journal: Journal::Wal, secure_delete: false, foreign_keys: false, synchronous_normal: false, quick_check: true, quick_check_max_bytes: QUICK_CHECK_MAX_BYTES, backup: true, recover: true }
    }

    /// No write-ahead log and overwritten deletions: for what the person asked to forget.
    pub fn private() -> Self {
        Self { busy_ms: 5_000, journal: Journal::Delete, secure_delete: true, foreign_keys: false, synchronous_normal: false, quick_check: true, quick_check_max_bytes: QUICK_CHECK_MAX_BYTES, backup: false, recover: true }
    }

    pub fn busy(mut self, ms: u32) -> Self {
        self.busy_ms = ms;
        self
    }

    pub fn journal(mut self, journal: Journal) -> Self {
        self.journal = journal;
        self
    }

    pub fn secure_delete(mut self, on: bool) -> Self {
        self.secure_delete = on;
        self
    }

    pub fn foreign_keys(mut self, on: bool) -> Self {
        self.foreign_keys = on;
        self
    }

    pub fn synchronous_normal(mut self, on: bool) -> Self {
        self.synchronous_normal = on;
        self
    }

    pub fn quick_check(mut self, on: bool) -> Self {
        self.quick_check = on;
        self
    }

    pub fn quick_check_up_to(mut self, bytes: u64) -> Self {
        self.quick_check_max_bytes = bytes;
        self
    }

    pub fn recover(mut self, on: bool) -> Self {
        self.recover = on;
        self
    }

    pub fn backup(mut self, on: bool) -> Self {
        self.backup = on;
        self
    }
}

/// One step of a schema. `id` counts up from 1 and is what `PRAGMA user_version` holds after the step.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub id: u32,
    pub sql: &'static str,
}

/// Looks at a database that has tables and no version and says which version it is really at (0: none of the steps). Used once, to stamp it.
pub type Adopt = fn(&Connection) -> u32;

#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    /// The file holds something that is not a healthy database (what the check said). Handled inside [`open`]; shown only if the file cannot be moved aside.
    Corrupt(String),
    /// The database is from a newer build than this one. Nothing was written.
    TooNew { path: Option<PathBuf>, found: u32, known: u32 },
    /// A step failed; it and the version were rolled back.
    Step { id: u32, error: rusqlite::Error },
    /// The copy before a migration could not be made, so the migration did not start.
    Backup(String),
    /// The list of steps is wrong (not counting up from 1): a programming error.
    Steps(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sqlite(e) => write!(f, "{e}"),
            StoreError::Io(e) => write!(f, "{e}"),
            StoreError::Corrupt(why) => write!(f, "the database is damaged ({why})"),
            StoreError::TooNew { path, found, known } => write!(
                f,
                "{}this database is newer than this build (it is at version {found}, this build knows up to {known}); it was not opened for writing, so nothing in it was changed",
                path.as_ref().map(|p| format!("{}: ", p.display())).unwrap_or_default()
            ),
            StoreError::Step { id, error } => write!(f, "step {id} of the schema failed and was rolled back: {error}"),
            StoreError::Backup(why) => write!(f, "the copy made before the upgrade failed, so the database was not upgraded: {why}"),
            StoreError::Steps(why) => write!(f, "the list of schema steps is wrong: {why}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

impl From<StoreError> for String {
    fn from(e: StoreError) -> String {
        e.to_string()
    }
}

impl StoreError {
    /// Is this the file being unfit to be a database (as opposed to locked, missing, or a failed step)? Only this moves a file aside.
    pub fn is_corruption(&self) -> bool {
        match self {
            StoreError::Corrupt(_) => true,
            StoreError::Sqlite(rusqlite::Error::SqliteFailure(f, _)) => matches!(f.code, ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt),
            _ => false,
        }
    }
}

/// An opened database. `recovered_from` is where the damaged file went, when there was one: the caller says so (a warning, a notice), because a store that starts
/// empty after a recovery should not do so silently.
#[derive(Debug)]
pub struct Opened {
    pub conn: Connection,
    pub recovered_from: Option<PathBuf>,
}

fn unix_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

/// The per-connection pragmas (those that do not need the file to be healthy).
fn per_connection(conn: &Connection, opts: &Opts) -> rusqlite::Result<()> {
    conn.busy_timeout(Duration::from_millis(opts.busy_ms as u64))?;
    if opts.secure_delete {
        conn.execute_batch("PRAGMA secure_delete = ON;")?;
    }
    if opts.foreign_keys {
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    }
    if opts.synchronous_normal {
        conn.execute_batch("PRAGMA synchronous = NORMAL;")?;
    }
    Ok(())
}

/// Light open: the pragmas of a connection and nothing else (no check, no journal change, no migration). For a store that opens a connection for every call;
/// the file was prepared by [`open`] or [`open_migrated`] when the store started.
pub fn connect(path: &Path, opts: &Opts) -> Result<Connection, StoreError> {
    let conn = Connection::open(path)?;
    per_connection(&conn, opts)?;
    Ok(conn)
}

/// The version a list of steps ends at (0 for none).
pub fn newest(steps: &[Migration]) -> u32 {
    steps.last().map(|s| s.id).unwrap_or(0)
}

/// [`connect`] for a store that opens a connection for every call: the call is refused when the file's version is above `known` (the newest step this build has: [`newest`]), which
/// costs one `PRAGMA user_version` read, a few microseconds. Without it, a store whose start-up was refused for a newer file would go on reading and writing that file call by call.
pub fn connect_checked(path: &Path, opts: &Opts, known: u32) -> Result<Connection, StoreError> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_millis(opts.busy_ms as u64))?;
    let found = version_of(&conn)?;
    if found > known {
        return Err(StoreError::TooNew { path: Some(path.to_path_buf()), found, known });
    }
    per_connection(&conn, opts)?;
    Ok(conn)
}

/// Opens (and if need be recovers) a database: see the module comment.
pub fn open(path: &Path, opts: &Opts) -> Result<Opened, StoreError> {
    open_inner(path, opts, None, None)
}

/// [`open`], then [`migrate`] to the newest of `steps`. A database newer than `steps` is refused before its journal mode or anything else is touched.
pub fn open_migrated(path: &Path, opts: &Opts, steps: &[Migration]) -> Result<Opened, StoreError> {
    open_inner(path, opts, Some(steps), None)
}

/// [`open_migrated`] for a store whose old builds changed the schema without a version: `adopt` stamps such a database.
pub fn open_migrated_adopting(path: &Path, opts: &Opts, steps: &[Migration], adopt: Adopt) -> Result<Opened, StoreError> {
    open_inner(path, opts, Some(steps), Some(adopt))
}

fn open_inner(path: &Path, opts: &Opts, steps: Option<&[Migration]>, adopt: Option<Adopt>) -> Result<Opened, StoreError> {
    if let Some(steps) = steps {
        validate_steps(steps)?;
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let (mut conn, recovered_from) = match prepare(path, opts, steps) {
        Ok(conn) => (conn, None),
        Err(e) if opts.recover && e.is_corruption() && path.is_file() => {
            let moved = move_aside(path).map_err(|io| StoreError::Corrupt(format!("{e}; and the file could not be moved aside: {io}")))?;
            tracing::warn!(
                "{} was damaged ({e}); it was moved to {} and a new one was made. Nothing was deleted: keep the old file as long as you may need what it held, and delete it when you do not (`aqua-cli doctor` lists it)",
                path.display(),
                moved.display()
            );
            (prepare(path, opts, steps)?, Some(moved))
        }
        Err(e) => return Err(e),
    };
    if let Some(steps) = steps {
        migrate_inner(&mut conn, Some(path), steps, adopt, opts.backup)?;
    }
    Ok(Opened { conn, recovered_from })
}

/// Connection, check, version guard, then the journal mode and the other pragmas. In that order: everything before the journal mode only reads, so a database
/// that is refused (too new) is byte for byte what it was.
fn prepare(path: &Path, opts: &Opts, steps: Option<&[Migration]>) -> Result<Connection, StoreError> {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let existed = size > 0;
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_millis(opts.busy_ms as u64))?;
    if existed {
        if opts.quick_check && size <= opts.quick_check_max_bytes {
            let verdict: String = conn.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
            if !verdict.eq_ignore_ascii_case("ok") {
                return Err(StoreError::Corrupt(verdict.chars().take(200).collect()));
            }
        } else {
            // Reading the schema is enough to find a file that is not a database at all.
            conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0))?;
        }
    }
    if let Some(steps) = steps {
        let known = steps.last().map(|s| s.id).unwrap_or(0);
        let found = version_of(&conn)?;
        if found > known {
            return Err(StoreError::TooNew { path: Some(path.to_path_buf()), found, known });
        }
    }
    // Changing the journal mode does not go through the busy handler when another connection holds the file (two starts at once: the app and `aqua-cli`), so it is retried here.
    match opts.journal {
        Journal::Wal => {
            let _: String = retry_busy(opts, || conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0)))?;
        }
        Journal::Delete => {
            let was: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
            let _: String = retry_busy(opts, || conn.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0)))?;
            if opts.secure_delete {
                conn.execute_batch("PRAGMA secure_delete = ON;")?;
            }
            if was.eq_ignore_ascii_case("wal") {
                // What was forgotten before this stays in the free pages of the file: rewrite it.
                retry_busy(opts, || conn.execute_batch("VACUUM"))?;
            }
        }
        Journal::Keep => {}
    }
    per_connection(&conn, opts)?;
    Ok(conn)
}

/// Runs `f` again while the database is busy or locked, for as long as the store's busy timeout.
fn retry_busy<T>(opts: &Opts, mut f: impl FnMut() -> rusqlite::Result<T>) -> rusqlite::Result<T> {
    let deadline = Instant::now() + Duration::from_millis(opts.busy_ms as u64);
    loop {
        match f() {
            Err(rusqlite::Error::SqliteFailure(e, _)) if matches!(e.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(15));
            }
            other => return other,
        }
    }
}

/// Moves a database aside as `<name>.corrupt-<unix>` (and `-wal`, `-shm`). A name that is taken gets a counter.
fn move_aside(path: &Path) -> std::io::Result<PathBuf> {
    let stamp = unix_secs();
    for n in 0..1000u32 {
        let tag = if n == 0 { format!(".corrupt-{stamp}") } else { format!(".corrupt-{stamp}-{n}") };
        let aside = sibling(path, &tag);
        if aside.exists() {
            continue;
        }
        std::fs::rename(path, &aside)?;
        for extra in ["-wal", "-shm"] {
            let from = sibling(path, extra);
            if from.exists() {
                // A log that cannot follow its database is better gone than left to be replayed onto the new file.
                if std::fs::rename(&from, sibling(&aside, extra)).is_err() {
                    let _ = std::fs::remove_file(&from);
                }
            }
        }
        return Ok(aside);
    }
    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "no free name to move the file to"))
}

fn validate_steps(steps: &[Migration]) -> Result<(), StoreError> {
    let mut last = 0;
    for s in steps {
        if s.id <= last {
            return Err(StoreError::Steps(format!("step {} comes after step {last}; the numbers must count up from 1", s.id)));
        }
        last = s.id;
    }
    Ok(())
}

fn version_of(conn: &Connection) -> Result<u32, StoreError> {
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    u32::try_from(v).map_err(|_| StoreError::Corrupt(format!("schema version {v}")))
}

/// The schema version (`PRAGMA user_version`) of an open database; 0 for one that never had a step.
pub fn version(conn: &Connection) -> Result<u32, StoreError> {
    version_of(conn)
}

/// Applies the steps that are not applied yet. Returns the version the database is at afterwards.
pub fn migrate(conn: &mut Connection, path: Option<&Path>, steps: &[Migration]) -> Result<u32, StoreError> {
    migrate_adopting(conn, path, steps, None)
}

/// [`migrate`] for a store whose old builds changed the schema without a version.
pub fn migrate_adopting(conn: &mut Connection, path: Option<&Path>, steps: &[Migration], adopt: Option<Adopt>) -> Result<u32, StoreError> {
    migrate_inner(conn, path, steps, adopt, true)
}

fn migrate_inner(conn: &mut Connection, path: Option<&Path>, steps: &[Migration], adopt: Option<Adopt>, backup: bool) -> Result<u32, StoreError> {
    validate_steps(steps)?;
    let known = steps.last().map(|s| s.id).unwrap_or(0);
    let mut found = version_of(conn)?;
    if found > known {
        return Err(StoreError::TooNew { path: path.map(Path::to_path_buf), found, known });
    }
    if found == 0 {
        if let Some(adopt) = adopt {
            let has_tables: i64 = conn.query_row("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'", [], |r| r.get(0))?;
            if has_tables > 0 {
                let at = adopt(conn).min(known);
                if at > 0 {
                    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    tx.execute_batch(&format!("PRAGMA user_version = {at}"))?;
                    tx.commit()?;
                    found = at;
                }
            }
        }
    }
    if steps.iter().all(|s| s.id <= found) {
        return Ok(found);
    }
    // A database that holds a version is about to change shape: keep a copy of it first.
    if found >= 1 && backup {
        if let Some(path) = path {
            back_up(conn, path, found)?;
        }
    }
    for step in steps.iter().filter(|s| s.id > found) {
        // Immediate, so that two starts at once do not both run a step; the second waits (the busy timeout) and then sees the version already raised.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|e| StoreError::Step { id: step.id, error: e })?;
        let now: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(|e| StoreError::Step { id: step.id, error: e })?;
        if now >= step.id as i64 {
            continue;
        }
        tx.execute_batch(step.sql).map_err(|e| StoreError::Step { id: step.id, error: e })?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", step.id)).map_err(|e| StoreError::Step { id: step.id, error: e })?;
        tx.commit().map_err(|e| StoreError::Step { id: step.id, error: e })?;
    }
    version_of(conn)
}

/// `<file>.bak-v<version>`: a consistent copy (`VACUUM INTO` reads through the write-ahead log). Keeps the newest [`BACKUPS_KEPT`].
fn back_up(conn: &Connection, path: &Path, version: u32) -> Result<(), StoreError> {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        return Ok(());
    }
    if size > BACKUP_MAX_BYTES {
        tracing::warn!("{} is {} MB: no copy is made before the upgrade (the limit is {} MB)", path.display(), size / 1_048_576, BACKUP_MAX_BYTES / 1_048_576);
        return Ok(());
    }
    let backup = sibling(path, &format!(".bak-v{version}"));
    // Written under a name of its own and then moved into place: two starts at once (the app and `aqua-cli`, or two threads) each make a complete copy and the last one wins,
    // where two writers into one file made "table already exists". VACUUM INTO also refuses to write over a file, and a copy of the same version from an earlier try is replaced.
    static COPY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let part = sibling(path, &format!(".bak-v{version}.part-{}-{}", std::process::id(), COPY.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let target = part.to_string_lossy().into_owned();
    if let Err(e) = conn.execute("VACUUM INTO ?1", [target.as_str()]) {
        let _ = std::fs::remove_file(&part);
        return Err(StoreError::Backup(e.to_string()));
    }
    if let Err(e) = std::fs::rename(&part, &backup) {
        let _ = std::fs::remove_file(&part);
        return Err(StoreError::Backup(format!("{}: {e}", backup.display())));
    }
    prune_backups(path);
    Ok(())
}

fn prune_backups(path: &Path) {
    let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()).or(Some(Path::new("."))) else { return };
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else { return };
    let prefix = format!("{name}.bak-v");
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut found: Vec<(u64, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            let v = n.strip_prefix(&prefix)?;
            (!v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())).then(|| (v.parse::<u64>().unwrap_or(0), e.path()))
        })
        .collect();
    // The copies of the highest versions are the ones worth keeping.
    found.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, old) in found.into_iter().skip(BACKUPS_KEPT) {
        let _ = std::fs::remove_file(old);
    }
}

/// Files that a store set aside or kept, below `data_dir` (the folder itself and the folders in it, two levels deep): databases moved aside as damaged
/// (`*.corrupt-*`) and files moved aside by `fsutil::quarantine` (`*.bad-*`). Their `-wal` and `-shm` companions are not listed. The doctor shows them as a
/// warning; nothing here deletes them.
pub fn leftovers(data_dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            if kind.is_dir() {
                // Models and snapshots are big and hold no database of ours.
                if depth > 0 && !matches!(name.as_str(), "models" | "snapshots" | "trash" | "cache") {
                    walk(&entry.path(), depth - 1, out);
                }
            } else if kind.is_file() && is_leftover_name(&name) {
                out.push(entry.path());
            }
        }
    }
    let mut out = Vec::new();
    walk(data_dir, 2, &mut out);
    out.sort();
    out
}

/// `x.db.corrupt-1700000000` or `x.json.bad-1700000000123` (with an optional `-<n>`); not the `-wal` / `-shm` of a corrupt database.
pub fn is_leftover_name(name: &str) -> bool {
    for marker in [".corrupt-", ".bad-"] {
        if let Some(at) = name.rfind(marker) {
            let tail = &name[at + marker.len()..];
            let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
            let mut parts = tail.split('-');
            if at > 0 && parts.next().is_some_and(digits) && parts.all(digits) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
#[path = "sqlite_store_tests.rs"]
mod tests;

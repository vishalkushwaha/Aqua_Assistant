//! The SQLite side of the index: schema, every query, and nothing else.
//!
//! One writer connection (the worker thread) and any number of short-lived read connections. WAL mode, so readers never
//! block the writer. Rows hold a name and a parent, never a path; a path is built by walking up to the root row, so a
//! folder rename is one row and a drive letter change is one `volumes.mount`.

use super::platform::VolumeMap;
use super::rules::{self, FOLDER};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone)]
pub struct Row {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub root_id: i64,
    pub volume_id: i64,
    pub name: String,
    pub kind: i64,
    pub size: i64,
    pub mtime: i64,
    pub flags: i64,
    pub depth: i64,
    pub dir_mtime: i64,
}

#[derive(Debug, Clone)]
pub struct RootRow {
    pub id: i64,
    pub volume_id: i64,
    pub rel_path: String,
    pub tier: i64,
    pub max_depth: i64,
    pub user_added: bool,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: i64,
    pub name: String,
    pub name_norm: String,
    pub kind: i64,
    pub size: i64,
    pub flags: i64,
    pub depth: i64,
    pub tier: i64,
    pub last_opened: i64,
    pub open_count: i64,
    pub volume_id: i64,
}

pub struct Db {
    pub conn: Connection,
    guids: HashMap<i64, String>,
}

const ROW_COLS: &str = "id, parent_id, root_id, volume_id, name, kind, size, mtime, flags, depth, dir_mtime";

fn row_of(r: &rusqlite::Row) -> rusqlite::Result<Row> {
    Ok(Row {
        id: r.get(0)?,
        parent_id: r.get(1)?,
        root_id: r.get(2)?,
        volume_id: r.get(3)?,
        name: r.get(4)?,
        kind: r.get(5)?,
        size: r.get(6)?,
        mtime: r.get(7)?,
        flags: r.get(8)?,
        depth: r.get(9)?,
        dir_mtime: r.get(10)?,
    })
}

impl Db {
    /// Open (creating if needed) the writer connection. A database that fails its quick check is moved aside by the
    /// caller (`open_or_recover`).
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        Self::prepare(conn)
    }

    /// A read-only connection for lookups. It never writes: a stale row is reported to the worker, not fixed here.
    pub fn open_reader(path: &Path) -> Result<Db> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA query_only=ON; PRAGMA mmap_size=0;")?;
        Ok(Db { conn, guids: HashMap::new() })
    }

    fn prepare(conn: Connection) -> Result<Db> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // WAL: readers never wait for the writer. NORMAL: no fsync per commit (a crash loses at most the last few
        // folders, which are listed again). mmap off: mapped pages would be charged to Aqua's memory.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA mmap_size=0; PRAGMA cache_size=-8192;
             PRAGMA temp_store=FILE; PRAGMA secure_delete=ON; PRAGMA journal_size_limit=33554432;
             PRAGMA wal_autocheckpoint=1000; PRAGMA foreign_keys=OFF;",
        )?;
        let mut db = Db { conn, guids: HashMap::new() };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&mut self) -> Result<()> {
        self.conn.execute_batch("CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);")?;
        let version: i64 = self.get_meta("schema_version").and_then(|v| v.parse().ok()).unwrap_or(0);
        if version > SCHEMA_VERSION {
            anyhow::bail!("the index was written by a newer Aqua (schema {version}); leaving it alone");
        }
        if version < 1 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS volumes(
                    id INTEGER PRIMARY KEY, guid TEXT NOT NULL UNIQUE, online INTEGER NOT NULL DEFAULT 1, offline_scans INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE IF NOT EXISTS roots(
                    entry_id INTEGER PRIMARY KEY, volume_id INTEGER NOT NULL, rel_path TEXT NOT NULL, tier INTEGER NOT NULL,
                    max_depth INTEGER NOT NULL, user_added INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL DEFAULT 'ok');
                 CREATE TABLE IF NOT EXISTS entries(
                    id INTEGER PRIMARY KEY AUTOINCREMENT, parent_id INTEGER, root_id INTEGER NOT NULL, volume_id INTEGER NOT NULL,
                    name TEXT NOT NULL, name_key TEXT NOT NULL, name_norm TEXT NOT NULL, kind INTEGER NOT NULL,
                    size INTEGER NOT NULL DEFAULT 0, mtime INTEGER NOT NULL DEFAULT 0, flags INTEGER NOT NULL DEFAULT 0,
                    depth INTEGER NOT NULL DEFAULT 0, dir_mtime INTEGER NOT NULL DEFAULT 0, scanned_at INTEGER NOT NULL DEFAULT 0,
                    last_opened INTEGER NOT NULL DEFAULT 0, open_count INTEGER NOT NULL DEFAULT 0);
                 CREATE INDEX IF NOT EXISTS entries_parent ON entries(parent_id, name_key);
                 CREATE INDEX IF NOT EXISTS entries_kind ON entries(kind);
                 CREATE INDEX IF NOT EXISTS entries_scanned ON entries(scanned_at);
                 CREATE INDEX IF NOT EXISTS entries_norm ON entries(name_norm);
                 CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(name_norm, tokenize='trigram');
                 CREATE TABLE IF NOT EXISTS todo(
                    seq INTEGER PRIMARY KEY AUTOINCREMENT, dir_id INTEGER NOT NULL UNIQUE, prio INTEGER NOT NULL);
                 CREATE INDEX IF NOT EXISTS todo_order ON todo(prio, seq);",
            )?;
            self.set_meta("schema_version", &SCHEMA_VERSION.to_string())?;
        }
        Ok(())
    }

    /// Fast structural check. False means the file cannot be trusted.
    pub fn quick_check_ok(&self) -> bool {
        self.conn.query_row("PRAGMA quick_check(1)", [], |r| r.get::<_, String>(0)).map(|s| s == "ok").unwrap_or(false)
    }

    pub fn begin(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        Ok(())
    }
    pub fn commit(&self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }
    pub fn rollback(&self) {
        let _ = self.conn.execute_batch("ROLLBACK");
    }

    // ---------------------------------------------------------------- meta
    pub fn get_meta(&self, key: &str) -> Option<String> {
        self.conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0)).optional().ok().flatten()
    }
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value])?;
        Ok(())
    }
    pub fn load_settings(&self) -> rules::Settings {
        self.get_meta("settings").and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default()
    }
    pub fn save_settings(&self, s: &rules::Settings) -> Result<()> {
        self.set_meta("settings", &serde_json::to_string(s)?)
    }

    // ---------------------------------------------------------------- volumes
    pub fn volume_id(&mut self, guid: &str) -> Result<i64> {
        self.conn.execute("INSERT OR IGNORE INTO volumes(guid) VALUES(?1)", [guid])?;
        let id: i64 = self.conn.query_row("SELECT id FROM volumes WHERE guid=?1", [guid], |r| r.get(0))?;
        self.guids.insert(id, guid.to_string());
        Ok(id)
    }
    pub fn volume_guid(&mut self, id: i64) -> Option<String> {
        if let Some(g) = self.guids.get(&id) {
            return Some(g.clone());
        }
        let g: Option<String> = self.conn.query_row("SELECT guid FROM volumes WHERE id=?1", [id], |r| r.get(0)).optional().ok().flatten();
        if let Some(g) = &g {
            self.guids.insert(id, g.clone());
        }
        g
    }
    pub fn set_volume_online(&self, id: i64, online: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE volumes SET online=?2, offline_scans=CASE WHEN ?2=1 THEN 0 ELSE offline_scans+1 END WHERE id=?1",
            params![id, online as i64],
        )?;
        Ok(())
    }
    pub fn volumes(&self) -> Vec<(i64, String, bool, i64)> {
        let Ok(mut st) = self.conn.prepare("SELECT id, guid, online, offline_scans FROM volumes") else { return vec![] };
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? == 1, r.get(3)?)))
            .map(|it| it.flatten().collect())
            .unwrap_or_default()
    }

    // ---------------------------------------------------------------- roots
    pub fn roots(&self) -> Vec<RootRow> {
        let Ok(mut st) = self.conn.prepare("SELECT entry_id, volume_id, rel_path, tier, max_depth, user_added, status FROM roots") else { return vec![] };
        st.query_map([], |r| {
            Ok(RootRow { id: r.get(0)?, volume_id: r.get(1)?, rel_path: r.get(2)?, tier: r.get(3)?, max_depth: r.get(4)?, user_added: r.get::<_, i64>(5)? == 1, status: r.get(6)? })
        })
        .map(|it| it.flatten().collect())
        .unwrap_or_default()
    }

    /// Add a root folder (idempotent) and queue it. Returns the root's entry id.
    pub fn add_root(&mut self, volume_id: i64, rel_path: &str, tier: i64, max_depth: i64, user_added: bool) -> Result<i64> {
        let existing: Option<i64> = self
            .conn
            .query_row("SELECT entry_id FROM roots WHERE volume_id=?1 AND lower(rel_path)=lower(?2)", params![volume_id, rel_path], |r| r.get(0))
            .optional()?;
        if let Some(id) = existing {
            self.conn.execute("UPDATE roots SET tier=?2, max_depth=?3, status='ok' WHERE entry_id=?1", params![id, tier, max_depth])?;
            return Ok(id);
        }
        let name = rel_path.to_string();
        self.conn.execute(
            "INSERT INTO entries(parent_id, root_id, volume_id, name, name_key, name_norm, kind, depth) VALUES(NULL, 0, ?1, ?2, ?3, ?4, ?5, 0)",
            params![volume_id, name, name.to_lowercase(), rules::norm(&name), FOLDER],
        )?;
        let id = self.conn.last_insert_rowid();
        self.conn.execute("UPDATE entries SET root_id=?1 WHERE id=?1", [id])?;
        self.conn.execute(
            "INSERT INTO roots(entry_id, volume_id, rel_path, tier, max_depth, user_added) VALUES(?1,?2,?3,?4,?5,?6)",
            params![id, volume_id, rel_path, tier, max_depth, user_added as i64],
        )?;
        self.todo_push(id, tier)?;
        Ok(id)
    }

    pub fn set_root_status(&self, id: i64, status: &str) -> Result<()> {
        self.conn.execute("UPDATE roots SET status=?2 WHERE entry_id=?1", params![id, status])?;
        Ok(())
    }

    pub fn remove_root(&self, id: i64) -> Result<()> {
        self.delete_subtree(id)?;
        self.conn.execute("DELETE FROM roots WHERE entry_id=?1", [id])?;
        Ok(())
    }

    // ---------------------------------------------------------------- entries
    pub fn entry(&self, id: i64) -> Option<Row> {
        self.conn.query_row(&format!("SELECT {ROW_COLS} FROM entries WHERE id=?1"), [id], row_of).optional().ok().flatten()
    }

    pub fn children(&self, dir_id: i64) -> Vec<Row> {
        let Ok(mut st) = self.conn.prepare_cached(&format!("SELECT {ROW_COLS} FROM entries WHERE parent_id=?1")) else { return vec![] };
        st.query_map([dir_id], row_of).map(|it| it.flatten().collect()).unwrap_or_default()
    }

    pub fn child_by_key(&self, parent_id: i64, key: &str) -> Option<i64> {
        self.conn.query_row("SELECT id FROM entries WHERE parent_id=?1 AND name_key=?2 LIMIT 1", params![parent_id, key], |r| r.get(0)).optional().ok().flatten()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_entry(&self, parent: &Row, name: &str, kind: i64, size: i64, mtime: i64, flags: i64, dir_mtime: i64) -> Result<i64> {
        let norm = rules::search_text(name, kind == FOLDER);
        self.conn.prepare_cached(
            "INSERT INTO entries(parent_id, root_id, volume_id, name, name_key, name_norm, kind, size, mtime, flags, depth, dir_mtime)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        )?
        .execute(params![parent.id, parent.root_id, parent.volume_id, name, name.to_lowercase(), norm, kind, size, mtime, flags, parent.depth + 1, dir_mtime])?;
        let id = self.conn.last_insert_rowid();
        if !norm.is_empty() {
            self.conn.prepare_cached("INSERT INTO entries_fts(rowid, name_norm) VALUES(?1, ?2)")?.execute(params![id, norm])?;
        }
        Ok(id)
    }

    pub fn update_entry(&self, id: i64, kind: i64, size: i64, mtime: i64, flags: i64, dir_mtime: Option<i64>) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE entries SET kind=?2, size=?3, mtime=?4, flags=?5, dir_mtime=COALESCE(?6, dir_mtime) WHERE id=?1")?
            .execute(params![id, kind, size, mtime, flags, dir_mtime])?;
        Ok(())
    }

    /// A rename or a case change: new name, same row (so usage history and, for a folder, all children are kept).
    pub fn rename_entry(&self, id: i64, name: &str, is_dir: bool) -> Result<()> {
        let norm = rules::search_text(name, is_dir);
        self.conn.execute("UPDATE entries SET name=?2, name_key=?3, name_norm=?4 WHERE id=?1", params![id, name, name.to_lowercase(), norm])?;
        self.conn.execute("DELETE FROM entries_fts WHERE rowid=?1", [id])?;
        if !norm.is_empty() {
            self.conn.execute("INSERT INTO entries_fts(rowid, name_norm) VALUES(?1, ?2)", params![id, norm])?;
        }
        Ok(())
    }

    pub fn set_dir_state(&self, id: i64, dir_mtime: i64, now: i64) -> Result<()> {
        self.conn.prepare_cached("UPDATE entries SET dir_mtime=?2, scanned_at=?3 WHERE id=?1")?.execute(params![id, dir_mtime, now])?;
        Ok(())
    }

    /// Delete an entry and everything below it, from the index, the search table and the work queue.
    pub fn delete_subtree(&self, id: i64) -> Result<usize> {
        let cte = "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION ALL SELECT e.id FROM entries e JOIN sub ON e.parent_id=sub.id)";
        self.conn.execute(&format!("{cte} DELETE FROM entries_fts WHERE rowid IN (SELECT id FROM sub)"), [id])?;
        self.conn.execute(&format!("{cte} DELETE FROM todo WHERE dir_id IN (SELECT id FROM sub)"), [id])?;
        let n = self.conn.execute(&format!("{cte} DELETE FROM entries WHERE id IN (SELECT id FROM sub)"), [id])?;
        Ok(n)
    }

    /// Delete every entry of one kind (ordinary files, when that setting is switched off).
    pub fn delete_kind(&self, kind: i64) -> Result<usize> {
        self.conn.execute("DELETE FROM entries_fts WHERE rowid IN (SELECT id FROM entries WHERE kind=?1)", [kind])?;
        Ok(self.conn.execute("DELETE FROM entries WHERE kind=?1", [kind])?)
    }

    pub fn clear_all(&self) -> Result<()> {
        self.conn.execute_batch("DELETE FROM entries_fts; DELETE FROM entries; DELETE FROM roots; DELETE FROM todo; DELETE FROM meta WHERE key IN ('catchup_cursor','last_verify');")?;
        Ok(())
    }

    pub fn mark_opened(&self, id: i64, now: i64) -> Result<()> {
        self.conn.execute("UPDATE entries SET last_opened=?2, open_count=open_count+1 WHERE id=?1", params![id, now])?;
        Ok(())
    }

    // ---------------------------------------------------------------- work queue
    pub fn todo_push(&self, dir_id: i64, prio: i64) -> Result<()> {
        self.conn
            .prepare_cached("INSERT INTO todo(dir_id, prio) VALUES(?1,?2) ON CONFLICT(dir_id) DO UPDATE SET prio=MIN(prio, excluded.prio)")?
            .execute(params![dir_id, prio])?;
        Ok(())
    }
    pub fn todo_pop(&self) -> Option<i64> {
        let id: Option<i64> = self.conn.query_row("SELECT dir_id FROM todo ORDER BY prio, seq LIMIT 1", [], |r| r.get(0)).optional().ok().flatten();
        if let Some(id) = id {
            let _ = self.conn.execute("DELETE FROM todo WHERE dir_id=?1", [id]);
        }
        id
    }
    pub fn todo_len(&self) -> i64 {
        self.conn.query_row("SELECT COUNT(*) FROM todo", [], |r| r.get(0)).unwrap_or(0)
    }
    pub fn todo_len_up_to(&self, prio: i64) -> i64 {
        self.conn.query_row("SELECT COUNT(*) FROM todo WHERE prio<=?1", [prio], |r| r.get(0)).unwrap_or(0)
    }

    /// Folders of tier 1 and 2 roots after `cursor`, for the startup catch-up.
    pub fn folders_after(&self, cursor: i64, limit: i64) -> Vec<(i64, i64)> {
        let Ok(mut st) = self.conn.prepare(
            "SELECT e.id, e.dir_mtime FROM entries e JOIN roots r ON r.entry_id=e.root_id
             WHERE e.kind=1 AND e.id>?1 AND r.tier<=2 ORDER BY e.id LIMIT ?2",
        ) else { return vec![] };
        st.query_map(params![cursor, limit], |r| Ok((r.get(0)?, r.get(1)?))).map(|it| it.flatten().collect()).unwrap_or_default()
    }

    /// Every folder of the tier 1 and 2 roots, for "look at everything again" (a file option was switched on).
    pub fn folder_ids(&self, max_tier: i64) -> Vec<i64> {
        let Ok(mut st) = self.conn.prepare("SELECT e.id FROM entries e JOIN roots r ON r.entry_id=e.root_id WHERE e.kind=1 AND r.tier<=?1") else { return vec![] };
        st.query_map([max_tier], |r| r.get(0)).map(|it| it.flatten().collect()).unwrap_or_default()
    }

    /// Folders not listed since `before`, oldest first, for the weekly verify.
    pub fn stale_folders(&self, before: i64, limit: i64) -> Vec<i64> {
        let Ok(mut st) = self.conn.prepare("SELECT id FROM entries WHERE kind=1 AND scanned_at>0 AND scanned_at<?1 ORDER BY scanned_at LIMIT ?2") else { return vec![] };
        st.query_map(params![before, limit], |r| r.get(0)).map(|it| it.flatten().collect()).unwrap_or_default()
    }

    // ---------------------------------------------------------------- paths
    /// The folder or file's path now, or None when its volume is not connected (or the chain is broken).
    pub fn path_of(&mut self, id: i64, vols: &VolumeMap) -> Option<PathBuf> {
        let mut names: Vec<String> = Vec::new();
        let mut cur = id;
        for _ in 0..256 {
            let (parent, name, vol): (Option<i64>, String, i64) = self
                .conn
                .query_row("SELECT parent_id, name, volume_id FROM entries WHERE id=?1", [cur], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()
                .ok()
                .flatten()?;
            match parent {
                None => {
                    let guid = self.volume_guid(vol)?;
                    let mut p = vols.mount_of(&guid)?;
                    if !name.is_empty() {
                        p.push(&name);
                    }
                    for n in names.iter().rev() {
                        p.push(n);
                    }
                    return Some(p);
                }
                Some(pid) => {
                    names.push(name);
                    cur = pid;
                }
            }
        }
        None
    }

    // ---------------------------------------------------------------- search
    /// Candidates whose search text contains the query. `kinds` limits what is returned. Shorter names first, so the
    /// 200 kept before ranking are the closest ones, not an arbitrary 200.
    pub fn search(&self, query_norm: &str, words: &[String], kinds: &[i64], limit: i64) -> Vec<Candidate> {
        if query_norm.is_empty() {
            return vec![];
        }
        let kind_list = kinds.iter().map(|k| k.to_string()).collect::<Vec<_>>().join(",");
        let cols = "e.id, e.name, e.name_norm, e.kind, e.size, e.flags, e.depth, COALESCE(r.tier,3), e.last_opened, e.open_count, e.volume_id";
        let read = |r: &rusqlite::Row| -> rusqlite::Result<Candidate> {
            Ok(Candidate { id: r.get(0)?, name: r.get(1)?, name_norm: r.get(2)?, kind: r.get(3)?, size: r.get(4)?, flags: r.get(5)?, depth: r.get(6)?, tier: r.get(7)?, last_opened: r.get(8)?, open_count: r.get(9)?, volume_id: r.get(10)? })
        };
        let run = |sql: String, arg: String| -> Vec<Candidate> {
            let Ok(mut st) = self.conn.prepare(&sql) else { return vec![] };
            st.query_map([arg], read).map(|it| it.flatten().collect()).unwrap_or_default()
        };
        let chars = query_norm.chars().count();
        if chars < 3 {
            let like = format!("{}%", query_norm.replace('%', "").replace('_', ""));
            return run(
                format!("SELECT {cols} FROM entries e LEFT JOIN roots r ON r.entry_id=e.root_id WHERE e.name_norm LIKE ?1 AND e.kind IN ({kind_list}) ORDER BY length(e.name_norm) LIMIT {limit}"),
                like,
            );
        }
        // The whole query as one phrase (substring of the normalised name).
        let phrase = format!("\"{}\"", query_norm.replace('"', ""));
        let mut out = run(
            format!("SELECT {cols} FROM entries_fts f JOIN entries e ON e.id=f.rowid LEFT JOIN roots r ON r.entry_id=e.root_id WHERE entries_fts MATCH ?1 AND e.kind IN ({kind_list}) ORDER BY length(e.name_norm) LIMIT {limit}"),
            phrase,
        );
        // Few hits and several words: all the words, in any order ("speed need").
        let long_words: Vec<String> = words.iter().map(|w| rules::norm(w)).filter(|w| w.chars().count() >= 3).collect();
        if out.len() < 3 && long_words.len() > 1 {
            let all = long_words.iter().map(|w| format!("\"{}\"", w.replace('"', ""))).collect::<Vec<_>>().join(" AND ");
            for c in run(
                format!("SELECT {cols} FROM entries_fts f JOIN entries e ON e.id=f.rowid LEFT JOIN roots r ON r.entry_id=e.root_id WHERE entries_fts MATCH ?1 AND e.kind IN ({kind_list}) ORDER BY length(e.name_norm) LIMIT {limit}"),
                all,
            ) {
                if !out.iter().any(|o| o.id == c.id) {
                    out.push(c);
                }
            }
        }
        out
    }

    pub fn counts(&self) -> (i64, i64, i64) {
        let q = |sql: &str| -> i64 { self.conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
        (q("SELECT COUNT(*) FROM entries WHERE kind=1"), q("SELECT COUNT(*) FROM entries WHERE kind IN (2,3,4,5)"), q("SELECT COUNT(*) FROM entries WHERE kind=6"))
    }
}

/// Open the writer database. If the file is damaged it is moved aside (kept for inspection) and a fresh one is made.
pub fn open_or_recover(path: &Path) -> Result<(Db, bool)> {
    match Db::open(path) {
        Ok(db) if db.quick_check_ok() => Ok((db, false)),
        Ok(db) => {
            drop(db);
            move_aside(path)?;
            Ok((Db::open(path)?, true))
        }
        Err(e) if e.to_string().contains("newer Aqua") => Err(e),
        Err(_) => {
            move_aside(path)?;
            Ok((Db::open(path)?, true))
        }
    }
}

fn move_aside(path: &Path) -> Result<()> {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let bad = path.with_extension(format!("db.bad-{stamp}"));
    std::fs::rename(path, &bad).or_else(|_| std::fs::remove_file(path))?;
    for ext in ["db-wal", "db-shm"] {
        let _ = std::fs::remove_file(path.with_extension(ext));
    }
    Ok(())
}

/// Delete the database and every file that belongs to it (WAL, shared memory, moved-aside copies).
pub fn delete_files(path: &Path) {
    for ext in ["db", "db-wal", "db-shm", "db-journal"] {
        let _ = std::fs::remove_file(path.with_extension(ext));
    }
    if let Some(dir) = path.parent() {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().contains(".db.bad-") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

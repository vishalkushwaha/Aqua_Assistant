//! The comment inbox: drafted replies waiting for a person, and what became of each.
//!
//! A draft moves through one set of states, and only forward through the doors below, so a reply can never be sent
//! twice and nothing is posted that a person did not approve:
//!
//! ```text
//! pending --approve--> posting --> posted
//!    |                    |------> unknown  (sent, but not seen on the page; a person checks)
//!    |                    '------> failed   (nothing was sent)
//!    '--skip--> skipped         unknown/failed --"it was posted"--> posted
//!                               unknown/failed --"try again"-----> pending
//! ```
//!
//! `posting` is written *before* the browser is touched and is taken with a single conditional update, so two
//! approvals of the same draft cannot both send it. If Aqua stops while a reply is being sent, the draft is found in
//! `posting` at the next start and becomes `unknown` (never retried by itself).
//!
//! A comment has at most one draft, ever (`post_url` + `comment_id` is unique), so a comment that was dealt with is
//! not drafted again when the post is read again.

use crate::flows::browser_exec::Driver;
use crate::flows::run::exec::NodeError;
use crate::flows::social::{self, comments::clean};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub type R<T> = Result<T, String>;

fn db(e: rusqlite::Error) -> String {
    format!("flow store: {e}")
}

pub fn init_schema(c: &Connection) -> R<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS flow_comment_drafts (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             owner_id TEXT NOT NULL,
             flow_id INTEGER NOT NULL,
             run_id INTEGER NOT NULL,
             post_url TEXT NOT NULL,
             my_name TEXT NOT NULL DEFAULT '',
             comment_id TEXT NOT NULL,
             author TEXT NOT NULL,
             comment_text TEXT NOT NULL,
             category TEXT NOT NULL DEFAULT '',
             reply TEXT NOT NULL,
             note TEXT NOT NULL DEFAULT '',
             sent_text TEXT NOT NULL DEFAULT '',
             status TEXT NOT NULL DEFAULT 'pending',
             error TEXT NOT NULL DEFAULT '',
             reply_id TEXT NOT NULL DEFAULT '',
             created_at INTEGER NOT NULL,
             decided_at INTEGER NOT NULL DEFAULT 0,
             UNIQUE (owner_id, post_url, comment_id)
         );
         CREATE INDEX IF NOT EXISTS idx_comment_drafts ON flow_comment_drafts(owner_id, status, id);",
    )
    .map_err(db)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Draft {
    pub id: i64,
    pub flow_id: i64,
    pub run_id: i64,
    pub post_url: String,
    /// The post owner's name, used to tell their own comments from others'.
    pub my_name: String,
    pub comment_id: String,
    pub author: String,
    pub comment_text: String,
    pub category: String,
    pub reply: String,
    pub note: String,
    /// What was actually sent (the draft as the person edited it).
    pub sent_text: String,
    /// pending | posting | posted | unknown | failed | skipped
    pub status: String,
    pub error: String,
    pub reply_id: String,
    pub created_at: i64,
    pub decided_at: i64,
}

/// A draft as the draft step hands it over.
#[derive(Debug, Clone)]
pub struct NewDraft {
    pub comment_id: String,
    pub author: String,
    pub comment_text: String,
    pub category: String,
    pub reply: String,
    pub note: String,
}

const COLS: &str = "id, flow_id, run_id, post_url, my_name, comment_id, author, comment_text, category, reply, note, sent_text, status, error, reply_id, created_at, decided_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Draft> {
    Ok(Draft { id: r.get(0)?, flow_id: r.get(1)?, run_id: r.get(2)?, post_url: r.get(3)?, my_name: r.get(4)?, comment_id: r.get(5)?, author: r.get(6)?, comment_text: r.get(7)?, category: r.get(8)?, reply: r.get(9)?, note: r.get(10)?, sent_text: r.get(11)?, status: r.get(12)?, error: r.get(13)?, reply_id: r.get(14)?, created_at: r.get(15)?, decided_at: r.get(16)? })
}

pub struct CommentInbox {
    path: PathBuf,
}

impl CommentInbox {
    pub fn new(path: impl AsRef<Path>) -> Self {
        let s = Self { path: path.as_ref().to_path_buf() };
        if let Err(e) = s.conn().and_then(|c| init_schema(&c)) {
            tracing::warn!("{e}");
        }
        s
    }

    fn conn(&self) -> R<Connection> {
        super::store::conn_at(&self.path)
    }

    /// The owner and flow of a run (drafts belong to whoever's run made them).
    fn run_owner(&self, run_id: i64) -> R<Option<(String, i64)>> {
        self.conn()?.query_row("SELECT owner_id, flow_id FROM flow_runs WHERE id=?1", params![run_id], |r| Ok((r.get(0)?, r.get(1)?))).optional().map_err(db)
    }

    /// The comments on this post that already have a draft, of any kind.
    pub fn known_comment_ids(&self, run_id: i64, post_url: &str) -> R<Vec<String>> {
        let Some((owner, _)) = self.run_owner(run_id)? else { return Ok(vec![]) };
        let c = self.conn()?;
        let mut stmt = c.prepare("SELECT comment_id FROM flow_comment_drafts WHERE owner_id=?1 AND post_url=?2").map_err(db)?;
        let rows = stmt.query_map(params![owner, post_url], |r| r.get::<_, String>(0)).map_err(db)?.collect::<Result<Vec<_>, _>>().map_err(db);
        rows
    }

    /// Put drafts in the inbox as pending. A comment that already has one is left alone. Returns (added, already there).
    pub fn add(&self, run_id: i64, post_url: &str, my_name: &str, drafts: &[NewDraft], at: i64) -> R<(usize, usize)> {
        let Some((owner, flow_id)) = self.run_owner(run_id)? else { return Err("no such run".into()) };
        let c = self.conn()?;
        let (mut added, mut there) = (0, 0);
        for d in drafts {
            let n = c
                .execute(
                    "INSERT OR IGNORE INTO flow_comment_drafts (owner_id, flow_id, run_id, post_url, my_name, comment_id, author, comment_text, category, reply, note, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                    params![owner, flow_id, run_id, post_url, my_name, d.comment_id, d.author, clean(&d.comment_text), d.category, d.reply, d.note, at],
                )
                .map_err(db)?;
            if n == 1 {
                added += 1;
            } else {
                there += 1;
            }
        }
        Ok((added, there))
    }

    pub fn get(&self, owner: &str, id: i64) -> R<Option<Draft>> {
        self.conn()?.query_row(&format!("SELECT {COLS} FROM flow_comment_drafts WHERE id=?1 AND owner_id=?2"), params![id, owner], row).optional().map_err(db)
    }

    pub fn list(&self, owner: &str, status: Option<&str>, limit: u32) -> R<Vec<Draft>> {
        let c = self.conn()?;
        let limit = limit.clamp(1, 500);
        match status {
            Some(s) => c.prepare(&format!("SELECT {COLS} FROM flow_comment_drafts WHERE owner_id=?1 AND status=?2 ORDER BY id DESC LIMIT ?3")).map_err(db)?.query_map(params![owner, s, limit], row).map_err(db)?.collect::<Result<Vec<_>, _>>().map_err(db),
            None => c.prepare(&format!("SELECT {COLS} FROM flow_comment_drafts WHERE owner_id=?1 ORDER BY id DESC LIMIT ?2")).map_err(db)?.query_map(params![owner, limit], row).map_err(db)?.collect::<Result<Vec<_>, _>>().map_err(db),
        }
    }

    pub fn counts(&self, owner: &str) -> R<std::collections::BTreeMap<String, i64>> {
        let c = self.conn()?;
        let mut stmt = c.prepare("SELECT status, COUNT(*) FROM flow_comment_drafts WHERE owner_id=?1 GROUP BY status").map_err(db)?;
        let rows = stmt.query_map(params![owner], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).map_err(db)?.collect::<Result<Vec<_>, _>>().map_err(db)?;
        Ok(rows.into_iter().collect())
    }

    /// The only door to `posting`: takes the draft if, and only if, it is still waiting.
    pub(crate) fn begin(&self, owner: &str, id: i64, text: &str, at: i64) -> R<()> {
        let n = self.conn()?.execute("UPDATE flow_comment_drafts SET status='posting', sent_text=?1, decided_at=?2, error='' WHERE id=?3 AND owner_id=?4 AND status='pending'", params![text, at, id, owner]).map_err(db)?;
        if n == 1 {
            return Ok(());
        }
        match self.get(owner, id)? {
            None => Err("no such draft".into()),
            Some(d) => Err(format!("that draft is not waiting for a decision (it is {}), so nothing was posted", d.status)),
        }
    }

    fn finish(&self, id: i64, status: &str, error: &str, reply_id: &str) -> R<()> {
        self.conn()?.execute("UPDATE flow_comment_drafts SET status=?1, error=?2, reply_id=?3 WHERE id=?4 AND status='posting'", params![status, error, reply_id, id]).map_err(db)?;
        Ok(())
    }

    pub fn skip(&self, owner: &str, id: i64, at: i64) -> R<Draft> {
        let n = self.conn()?.execute("UPDATE flow_comment_drafts SET status='skipped', decided_at=?1 WHERE id=?2 AND owner_id=?3 AND status='pending'", params![at, id, owner]).map_err(db)?;
        let d = self.get(owner, id)?.ok_or("no such draft")?;
        if n == 0 {
            return Err(format!("that draft is not waiting for a decision (it is {})", d.status));
        }
        Ok(d)
    }

    /// A person looked at a reply that could not be confirmed (or that failed): it *was* posted, or it should go round again.
    pub fn resolve(&self, owner: &str, id: i64, action: &str, at: i64) -> R<Draft> {
        let new = match action {
            "posted" => "posted",
            "retry" => "pending",
            other => return Err(format!("choose \"posted\" or \"retry\", not \"{other}\"")),
        };
        let n = self.conn()?.execute("UPDATE flow_comment_drafts SET status=?1, decided_at=?2, error=CASE WHEN ?1='pending' THEN '' ELSE error END WHERE id=?3 AND owner_id=?4 AND status IN ('unknown','failed')", params![new, at, id, owner]).map_err(db)?;
        let d = self.get(owner, id)?.ok_or("no such draft")?;
        if n == 0 {
            return Err(format!("only a reply that could not be confirmed, or that failed, can be resolved (this one is {})", d.status));
        }
        Ok(d)
    }

    /// At start: a reply that was being sent when Aqua stopped may or may not have gone out.
    pub fn recover_crashed(&self) -> R<usize> {
        self.conn()?.execute("UPDATE flow_comment_drafts SET status='unknown', error='Aqua stopped while this reply was being sent, so it is not known whether it was posted. Check the post.' WHERE status='posting'", []).map_err(db)
    }

    pub fn delete_for_flow(&self, owner: &str, flow_id: i64) -> R<usize> {
        self.conn()?.execute("DELETE FROM flow_comment_drafts WHERE owner_id=?1 AND flow_id=?2", params![owner, flow_id]).map_err(db)
    }

    /// Approve a draft and post it. `text` is the reply as the person wants it (their edit); `None` sends the draft as
    /// written. Blocks while the browser works: call it from a blocking thread.
    pub fn approve_and_post(&self, driver: &dyn Driver, owner: &str, id: i64, text: Option<&str>, at: i64) -> R<Draft> {
        let d = self.get(owner, id)?.ok_or("no such draft")?;
        let send = clean(text.unwrap_or(&d.reply));
        if send.trim().is_empty() {
            return Err("the reply is empty, so nothing was posted".into());
        }
        if send.chars().count() > social::linkedin::MAX_REPLY_CHARS {
            return Err(format!("the reply is {} characters; LinkedIn takes at most {}", send.chars().count(), social::linkedin::MAX_REPLY_CHARS));
        }
        self.begin(owner, id, &send, at)?;
        // From here the reply may go out: whatever happens, the draft does not go back to "pending" by itself.
        match social::post_reply(driver, &d.post_url, &d.comment_id, &d.my_name, &send) {
            Ok(reply_id) => self.finish(id, "posted", "", &reply_id)?,
            Err(NodeError::Unknown(m)) => self.finish(id, "unknown", &crate::memory::experience::sanitize::redact(&m).0, "")?,
            Err(e) => self.finish(id, "failed", &crate::memory::experience::sanitize::redact(e.message()).0, "")?,
        }
        self.get(owner, id)?.ok_or_else(|| "the draft disappeared".into())
    }
}

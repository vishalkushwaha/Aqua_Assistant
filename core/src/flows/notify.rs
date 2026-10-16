//! Telling the person what happened: notifications, where they go, and the summary of a run.
//!
//! In-app notifications are stored and always available. Email, Slack and push need an account to be
//! connected (the integration architecture, not built yet): asking for one that is not connected does
//! not lose the message (it is stored in-app) and records that the other channel could not deliver, so
//! the settings page can say so instead of pretending.

use super::run::model::*;
use super::supervisor::{judge, Verdict, VerdictKind};
use super::zone::Zone;
use crate::flows::model::FlowDef;
use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub type R<T> = Result<T, String>;

fn db(e: rusqlite::Error) -> String {
    format!("flow store: {e}")
}

pub fn init_schema(c: &Connection) -> R<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS flow_notifications (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             owner_id TEXT NOT NULL,
             at INTEGER NOT NULL,
             kind TEXT NOT NULL,
             level TEXT NOT NULL,
             flow_id INTEGER NOT NULL,
             run_id INTEGER,
             schedule_id INTEGER,
             title TEXT NOT NULL,
             body TEXT NOT NULL,
             data_json TEXT NOT NULL DEFAULT 'null',
             read INTEGER NOT NULL DEFAULT 0,
             delivery_json TEXT NOT NULL DEFAULT '[]'
         );
         CREATE INDEX IF NOT EXISTS idx_notifications ON flow_notifications(owner_id, id);",
    )
    .map_err(db)
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Delivery {
    pub channel: String,
    pub ok: bool,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Notification {
    pub id: i64,
    pub owner_id: String,
    pub at: i64,
    /// started | succeeded | failed | recovered | needs_you | missed | step
    pub kind: String,
    /// info | success | warning | error
    pub level: String,
    pub flow_id: i64,
    pub run_id: Option<i64>,
    pub schedule_id: Option<i64>,
    pub title: String,
    pub body: String,
    pub data: Value,
    pub read: bool,
    pub delivery: Vec<Delivery>,
}

pub struct NewNotification {
    pub kind: &'static str,
    pub level: &'static str,
    pub flow_id: i64,
    pub run_id: Option<i64>,
    pub schedule_id: Option<i64>,
    pub title: String,
    pub body: String,
    pub data: Value,
}

pub struct NotifyStore {
    path: PathBuf,
}

impl NotifyStore {
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

    fn insert(&self, owner: &str, n: &NewNotification, at: i64, delivery: &[Delivery]) -> R<i64> {
        let c = self.conn()?;
        c.execute(
            "INSERT INTO flow_notifications (owner_id, at, kind, level, flow_id, run_id, schedule_id, title, body, data_json, delivery_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![owner, at, n.kind, n.level, n.flow_id, n.run_id, n.schedule_id, n.title, n.body.chars().take(6000).collect::<String>(), n.data.to_string(), serde_json::to_string(delivery).unwrap_or_default()],
        )
        .map_err(db)?;
        Ok(c.last_insert_rowid())
    }

    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Notification> {
        let delivery: Vec<(String, bool, String)> = serde_json::from_str::<Vec<Value>>(&r.get::<_, String>(12)?)
            .unwrap_or_default()
            .into_iter()
            .map(|v| (v["channel"].as_str().unwrap_or("").to_string(), v["ok"].as_bool().unwrap_or(false), v["note"].as_str().unwrap_or("").to_string()))
            .collect();
        Ok(Notification {
            id: r.get(0)?,
            owner_id: r.get(1)?,
            at: r.get(2)?,
            kind: r.get(3)?,
            level: r.get(4)?,
            flow_id: r.get(5)?,
            run_id: r.get(6)?,
            schedule_id: r.get(7)?,
            title: r.get(8)?,
            body: r.get(9)?,
            data: serde_json::from_str(&r.get::<_, String>(10)?).unwrap_or(Value::Null),
            read: r.get::<_, i64>(11)? != 0,
            delivery: delivery.into_iter().map(|(channel, ok, note)| Delivery { channel, ok, note }).collect(),
        })
    }

    const COLS: &'static str = "id, owner_id, at, kind, level, flow_id, run_id, schedule_id, title, body, data_json, read, delivery_json";

    /// The newest first.
    pub fn list(&self, owner: &str, unread_only: bool, limit: u32) -> R<Vec<Notification>> {
        let c = self.conn()?;
        let sql = format!("SELECT {} FROM flow_notifications WHERE owner_id=?1 {} ORDER BY id DESC LIMIT ?2", Self::COLS, if unread_only { "AND read=0" } else { "" });
        let mut stmt = c.prepare(&sql).map_err(db)?;
        let rows = stmt.query_map(params![owner, limit.clamp(1, 500)], Self::from_row).map_err(db)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db)
    }

    /// Has this run already produced a notification of this kind? (So a watchdog says it once.)
    pub fn exists_for_run(&self, owner: &str, run_id: i64, kind: &str) -> R<bool> {
        let c = self.conn()?;
        let n: i64 = c.query_row("SELECT COUNT(*) FROM flow_notifications WHERE owner_id=?1 AND run_id=?2 AND kind=?3", params![owner, run_id, kind], |r| r.get(0)).map_err(db)?;
        Ok(n > 0)
    }

    pub fn unread_count(&self, owner: &str) -> R<i64> {
        self.conn()?.query_row("SELECT COUNT(*) FROM flow_notifications WHERE owner_id=?1 AND read=0", params![owner], |r| r.get(0)).map_err(db)
    }

    /// Mark some (or, with no ids, all) as read. Returns how many changed.
    pub fn mark_read(&self, owner: &str, ids: &[i64]) -> R<usize> {
        let c = self.conn()?;
        if ids.is_empty() {
            return c.execute("UPDATE flow_notifications SET read=1 WHERE owner_id=?1 AND read=0", params![owner]).map_err(db);
        }
        let mut n = 0;
        for id in ids {
            n += c.execute("UPDATE flow_notifications SET read=1 WHERE owner_id=?1 AND id=?2 AND read=0", params![owner, id]).map_err(db)?;
        }
        Ok(n)
    }

    pub fn delete_for_flow(&self, owner: &str, flow_id: i64) -> R<usize> {
        self.conn()?.execute("DELETE FROM flow_notifications WHERE owner_id=?1 AND flow_id=?2", params![owner, flow_id]).map_err(db)
    }

    /// Forget notifications that have been read and are older than `before`, and read ones beyond the newest `keep` of each
    /// person. Unread ones are never dropped: they are what the person has not seen yet.
    pub fn prune(&self, keep: u32, before: i64) -> R<usize> {
        self.conn()?
            .execute(
                "DELETE FROM flow_notifications WHERE read=1 AND (at<?1 OR id NOT IN (SELECT id FROM flow_notifications n2 WHERE n2.owner_id=flow_notifications.owner_id ORDER BY id DESC LIMIT ?2))",
                params![before, keep],
            )
            .map_err(db)
    }
}

// ---- channels ---------------------------------------------------------------------------------------------

pub trait Channel: Send + Sync {
    fn id(&self) -> &'static str;
    fn label(&self) -> &'static str;
    fn connected(&self) -> bool;
    fn deliver(&self, n: &Notification) -> Result<(), String>;
}

/// A channel whose account is not connected: it says so instead of pretending to deliver.
struct NotConnected {
    id: &'static str,
    label: &'static str,
}

impl Channel for NotConnected {
    fn id(&self) -> &'static str {
        self.id
    }
    fn label(&self) -> &'static str {
        self.label
    }
    fn connected(&self) -> bool {
        false
    }
    fn deliver(&self, _n: &Notification) -> Result<(), String> {
        Err(format!("{} is not connected: connect an account to receive these there", self.label))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelStatus {
    pub id: String,
    pub label: String,
    pub connected: bool,
}

pub struct Notifier {
    pub store: Arc<NotifyStore>,
    channels: Vec<Arc<dyn Channel>>,
}

impl Notifier {
    pub fn new(store: Arc<NotifyStore>) -> Self {
        Self { store, channels: vec![Arc::new(NotConnected { id: "email", label: "Email" }), Arc::new(NotConnected { id: "slack", label: "Slack" }), Arc::new(NotConnected { id: "push", label: "Push notifications" })] }
    }

    /// Add a channel that can deliver (a connected account). Replaces the placeholder of the same id.
    pub fn with_channel(mut self, c: Arc<dyn Channel>) -> Self {
        self.channels.retain(|x| x.id() != c.id());
        self.channels.push(c);
        self
    }

    pub fn channel_status(&self) -> Vec<ChannelStatus> {
        let mut v = vec![ChannelStatus { id: "in_app".into(), label: "In Aqua".into(), connected: true }];
        v.extend(self.channels.iter().map(|c| ChannelStatus { id: c.id().into(), label: c.label().into(), connected: c.connected() }));
        v
    }

    /// Store the notification (always, in-app) and try each other channel that was asked for.
    pub fn emit(&self, owner: &str, channels: &[String], n: NewNotification, at: i64) -> R<Notification> {
        let id = self.store.insert(owner, &n, at, &[Delivery { channel: "in_app".into(), ok: true, note: String::new() }])?;
        let mut note = Notification { id, owner_id: owner.into(), at, kind: n.kind.into(), level: n.level.into(), flow_id: n.flow_id, run_id: n.run_id, schedule_id: n.schedule_id, title: n.title, body: n.body, data: n.data, read: false, delivery: vec![Delivery { channel: "in_app".into(), ok: true, note: String::new() }] };
        for wanted in channels.iter().filter(|c| c.as_str() != "in_app") {
            let d = match self.channels.iter().find(|c| c.id() == wanted) {
                Some(c) => match c.deliver(&note) {
                    Ok(()) => Delivery { channel: wanted.clone(), ok: true, note: String::new() },
                    Err(e) => Delivery { channel: wanted.clone(), ok: false, note: e },
                },
                None => Delivery { channel: wanted.clone(), ok: false, note: "unknown channel".into() },
            };
            note.delivery.push(d);
        }
        if note.delivery.len() > 1 {
            let c = self.store.conn()?;
            c.execute("UPDATE flow_notifications SET delivery_json=?1 WHERE id=?2", params![serde_json::to_string(&note.delivery).unwrap_or_default(), id]).map_err(db)?;
        }
        Ok(note)
    }
}

// ---- the summary of a run ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RunSummary {
    pub name: String,
    pub status: String,
    pub started_at: i64,
    pub finished_at: i64,
    pub duration_secs: i64,
    pub steps_total: usize,
    pub steps_succeeded: usize,
    pub steps_failed: usize,
    pub steps_skipped: usize,
    pub failed_step: Option<String>,
    pub error: String,
    /// The most useful single line: what came of it.
    pub result: String,
    /// (step, name of the value, the value cut short)
    pub artifacts: Vec<(String, String, String)>,
    pub next_run_at: Option<i64>,
    /// How it really ended, judged from evidence (see `supervisor::verdict`).
    pub verdict: Verdict,
}

const ARTIFACT_KEYS: &[&str] = &["post_url", "image", "file", "text", "summary", "status"];

pub fn summarize(run: &Run, nodes: &[NodeRun], def: &FlowDef, next_run_at: Option<i64>) -> RunSummary {
    let name_of = |id: &str| def.node(id).map(|n| n.name.clone()).unwrap_or_else(|| id.to_string());
    let count = |s: NodeStatus| nodes.iter().filter(|n| n.status == s).count();
    let failed = nodes.iter().find(|n| matches!(n.status, NodeStatus::Failed | NodeStatus::NeedsAction | NodeStatus::Interrupted));
    let mut artifacts = Vec::new();
    for n in nodes.iter().filter(|n| n.status == NodeStatus::Succeeded) {
        for k in ARTIFACT_KEYS {
            if let Some(v) = n.outputs.get(*k).and_then(|v| v.as_str()).filter(|v| !v.trim().is_empty()) {
                artifacts.push((name_of(&n.node_id), k.to_string(), v.chars().take(160).collect::<String>()));
            }
        }
    }
    let result = match run.status {
        RunStatus::Succeeded => nodes
            .iter()
            .rev()
            .filter(|n| n.status == NodeStatus::Succeeded)
            .find_map(|n| n.outputs.get("post_url").and_then(|v| v.as_str()).filter(|u| !u.is_empty()).map(|u| format!("Result: {u}")).or_else(|| n.outputs.get("status").and_then(|v| v.as_str()).filter(|s| !s.is_empty() && !s.starts_with("[dry run]")).map(|s| format!("Result: {s}"))))
            .unwrap_or_else(|| format!("All {} steps that ran completed.", count(NodeStatus::Succeeded))),
        RunStatus::Failed | RunStatus::Interrupted => match failed {
            Some(f) => format!("Stopped at \"{}\": {}", name_of(&f.node_id), if f.error.is_empty() { run.error.clone() } else { f.error.clone() }),
            None => run.error.clone(),
        },
        RunStatus::Waiting => "Waiting for you.".into(),
        RunStatus::Cancelled => "Stopped.".into(),
        _ => String::new(),
    };
    RunSummary {
        name: run.name.clone(),
        status: run.status.as_str().into(),
        started_at: run.started_at,
        finished_at: run.finished_at,
        duration_secs: if run.finished_at >= run.started_at && run.started_at > 0 { run.finished_at - run.started_at } else { 0 },
        steps_total: nodes.len(),
        steps_succeeded: count(NodeStatus::Succeeded),
        steps_failed: count(NodeStatus::Failed) + count(NodeStatus::NeedsAction),
        steps_skipped: count(NodeStatus::Skipped),
        failed_step: failed.map(|f| name_of(&f.node_id)),
        error: run.error.clone(),
        result,
        artifacts,
        next_run_at,
        verdict: judge(run, nodes, def),
    }
}

fn duration(secs: i64) -> String {
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

impl RunSummary {
    /// Plain text for a notification, times in `zone`.
    pub fn text(&self, zone: &Zone) -> String {
        let at = |t: i64| if t > 0 { zone.show(t) } else { "-".into() };
        let mut s = format!("Workflow: {}\nStarted: {}\nFinished: {}\nDuration: {}\nSteps: {} total, {} succeeded, {} failed, {} skipped\n", self.name, at(self.started_at), at(self.finished_at), duration(self.duration_secs), self.steps_total, self.steps_succeeded, self.steps_failed, self.steps_skipped);
        if !self.result.is_empty() {
            s.push_str(&format!("{}\n", self.result));
        }
        if matches!(self.verdict.kind, VerdictKind::Recovered | VerdictKind::Unknown) {
            s.push_str(&format!("Outcome: {}\n", self.verdict.reason));
        }
        if let Some(c) = self.verdict.class.filter(|_| self.verdict.kind == VerdictKind::Failed) {
            s.push_str(&format!("Kind of problem: {} ({})\n", c.label(), c.explain()));
        }
        if !self.artifacts.is_empty() {
            s.push_str("Produced:\n");
            for (step, key, v) in self.artifacts.iter().take(6) {
                s.push_str(&format!("  - {step} ({key}): {v}\n"));
            }
        }
        if let Some(n) = self.next_run_at {
            s.push_str(&format!("Next run: {}\n", zone.show(n)));
        }
        s
    }
}

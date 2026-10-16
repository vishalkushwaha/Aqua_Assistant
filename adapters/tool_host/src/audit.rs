//! Append-only, hash-chained record of every call, including refused ones.
//!
//! Each line carries the SHA-256 of the previous line, so editing, deleting or reordering a
//! line breaks the chain from that point on. The chain survives rotation: the first line of a
//! new file points at the last hash of the old one. The ledger detects tampering after the
//! fact; it cannot stop code that runs as the same user from rewriting the whole file.

use crate::heads::{self, Head, HeadSigner, GENESIS_HEAD, HEAD_EVERY};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;
use tracing::warn;

const ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// A single text argument longer than this is recorded as its first part plus its length and SHA-256.
pub const MAX_ARG_FIELD_CHARS: usize = 1000;
/// All the arguments of one call together may not take more than this many bytes of the log.
pub const MAX_ARGS_BYTES: usize = 16 * 1024;

fn sha_hex(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn shrink_value(v: &Value) -> Value {
    match v {
        Value::String(s) if s.chars().count() > MAX_ARG_FIELD_CHARS => {
            let head: String = s.chars().take(200).collect();
            Value::String(format!("{head}\u{2026} [{} chars, sha256 {}]", s.chars().count(), sha_hex(s)))
        }
        Value::Array(a) => Value::Array(a.iter().map(shrink_value).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), shrink_value(x))).collect()),
        other => other.clone(),
    }
}

/// The arguments of a call as the log keeps them (already redacted by the caller). A file written through `fs.write` can be
/// 1 MB and a burst of such calls would push older evidence out of the few rotated files within minutes, so large text is
/// kept as a short beginning plus its length and SHA-256: enough to show what was done and to match it against a file,
/// not enough to fill the log. Everything else is kept as it is, unless the whole record is still over [`MAX_ARGS_BYTES`].
pub fn shrink_args(v: &Value) -> Value {
    let small = shrink_value(v);
    let text = small.to_string();
    if text.len() > MAX_ARGS_BYTES {
        return serde_json::json!({ "truncated": true, "bytes": text.len(), "sha256": sha_hex(&text) });
    }
    small
}
/// A name or an identifier of a call (tool, session, request, run) longer than this is recorded as its beginning plus its length and
/// SHA-256. Chosen so that the cut form (at most about 175 characters) is itself within the limit and a second cut changes nothing.
pub const MAX_ID_CHARS: usize = 200;
/// Characters of a long identifier that are kept as it is.
const ID_HEAD_CHARS: usize = 80;
/// Permission classes recorded for one call (there are fewer than ten).
const MAX_CLASSES: usize = 16;

/// `text` as the log keeps it: as it is when it has at most `max` characters, else its first characters plus its length and SHA-256, so two
/// different long texts stay different. Meant for texts a caller chooses: the log writes a line for every call, including refused ones,
/// and a refused "unknown tool" used to keep whatever name it was given.
pub fn clip_field(text: &str, max: usize) -> String {
    if text.len() <= max || text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(ID_HEAD_CHARS.min(max)).collect();
    format!("{head}\u{2026} [{} chars, sha256 {}]", text.chars().count(), sha_hex(text))
}

const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// How a call was allowed or refused, as the ledger keeps it (card R6-12): a line that says only "approved" cannot be told apart from the person's answer to a
/// question and a grant that was given an hour ago. The note is part of the line, so the hash covers it; a line written before it existed has none, and verifies as it did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalNote {
    /// `policy` (the rules allowed it and nobody was asked), `once` (the person said yes to this call), `always` (yes, and remember it), `session` (a yes given earlier
    /// in the session covered it), `preapproved` (the person allowed this run's project checks in this folder ahead of time, card OA10-05; the reason names the pre-approval's short id),
    /// `run` (the person said yes to this edit and to later edits in its folder until the run ends, card OA6-05; the reason names the folder and the run), `run_granted` (an edit that went
    /// through on such a grant without a question; the reason names the folder, the run and how many edits the grant has let through), `run_grant_made`, `run_grant_suspended` and
    /// `run_grant_ended` (the lines of the grant itself, which are not calls: the tool is `run.edit_grant`), or `refused` (no, no answer in time, or the run was stopped while the question
    /// was open).
    pub kind: String,
    /// Who decided: `policy`, `ui`, `voice` or `user`.
    pub by: String,
    /// Why it was asked, or why it was decided as it was, in the host's words (masked like the arguments).
    pub reason: String,
}

/// The word of an [`ApprovalNote`] for the host's decision (`auto`, `approved`, `approved_session`, `approved_always`, `denied`, `timeout`, `cancelled`).
pub fn approval_kind(decision: &str) -> &'static str {
    match decision {
        "auto" => "policy",
        "approved" => "once",
        "approved_always" => "always",
        "approved_session" => "session",
        // A project check the person allowed ahead of time for one run (card OA10-05, WF-14): not a question that was asked, and not an answer given in this session.
        "preapproved" => "preapproved",
        // The person said yes to this edit and to later edits in its folder until the run ends (card OA6-05, ED-10), and a later edit that went through on that grant without a question.
        "approved_run" => "run",
        "run_granted" => "run_granted",
        _ => "refused",
    }
}

/// What the host records for a call. Arguments are stored already redacted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub request_id: String,
    pub session_id: String,
    pub agent_run_id: String,
    pub caller: String,
    pub tool: String,
    pub args: Value,
    /// `auto`, `approved`, `denied_by_policy`, `denied_by_user`, `approval_timeout`, `invalid`, `unknown_tool`, `unavailable`.
    pub decision: String,
    pub classes: Vec<String>,
    pub ok: bool,
    pub error_code: Option<String>,
    pub duration_ms: u64,
    /// Set when the capability-tracking rule (see `PolicyEngine::decide_caps`) would have decided differently from the one that was applied:
    /// `ask->always_ask (reason)`. Absent from every line written before it existed, and from every line where the two agreed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow: Option<String>,
    /// How the call was allowed or refused (see [`ApprovalNote`]). Absent from every line written before it existed, and from the lines of calls that were not decided
    /// by the policy engine (a direct action, a refused request that never reached it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalNote>,
}

impl AuditEvent {
    /// The same event with every text that a caller can make long cut to a size the log can bear, and the arguments shrunk (see
    /// [`shrink_args`]; shrinking twice changes nothing). The ledger does this for every line it writes, whoever the writer is, so the
    /// size of one line is bounded by this and not by what a request carried: before, a few requests with a 2 MB tool name (the default
    /// body limit) were enough to push all the older records out of the five kept files.
    pub fn bounded(&self) -> AuditEvent {
        AuditEvent {
            request_id: clip_field(&self.request_id, MAX_ID_CHARS),
            session_id: clip_field(&self.session_id, MAX_ID_CHARS),
            agent_run_id: clip_field(&self.agent_run_id, MAX_ID_CHARS),
            caller: clip_field(&self.caller, MAX_ID_CHARS),
            tool: clip_field(&self.tool, MAX_ID_CHARS),
            args: shrink_args(&self.args),
            decision: clip_field(&self.decision, MAX_ID_CHARS),
            classes: self.classes.iter().take(MAX_CLASSES).map(|c| clip_field(c, 40)).collect(),
            ok: self.ok,
            error_code: self.error_code.as_deref().map(|c| clip_field(c, 80)),
            duration_ms: self.duration_ms,
            shadow: self.shadow.as_deref().map(|s| clip_field(s, 240)),
            approval: self.approval.as_ref().map(|a| ApprovalNote { kind: clip_field(&a.kind, 20), by: clip_field(&a.by, 20), reason: clip_field(&a.reason, 240) }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Line {
    pub(crate) seq: u64,
    pub(crate) ts_unix_ms: u64,
    pub(crate) prev: String,
    #[serde(flatten)]
    pub(crate) event: AuditEvent,
    pub(crate) hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainError {
    pub line: usize,
    pub reason: String,
}

impl std::fmt::Display for ChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "audit chain broken at line {}: {}", self.line, self.reason)
    }
}

struct State {
    seq: u64,
    prev: String,
    /// The lines written since the last tree head, as (sequence number, line hash): what the next head will cover.
    batch: Vec<(u64, String)>,
    head_index: u64,
    last_head: String,
}

/// What log rotation has deleted since this ledger was opened (card OA1-04, SE-19): the oldest file goes when a new one is started, and its records are not kept anywhere else.
#[derive(Debug, Clone, Copy, Default)]
struct Lost {
    files: u64,
    /// The last sequence number that was in the newest file deleted.
    to_seq: u64,
}

pub struct AuditLedger {
    path: PathBuf,
    state: Mutex<State>,
    /// Signs the tree heads (see [`crate::heads`]). Without one the ledger is chained but no head is written.
    signer: RwLock<Option<Arc<dyn HeadSigner>>>,
    lost: Mutex<Lost>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn digest(prev: &str, seq: u64, ts: u64, event: &AuditEvent) -> String {
    let body = serde_json::to_string(event).unwrap_or_default();
    let mut h = Sha256::new();
    h.update(prev.as_bytes());
    h.update(b"\n");
    h.update(seq.to_string().as_bytes());
    h.update(b"\n");
    h.update(ts.to_string().as_bytes());
    h.update(b"\n");
    h.update(body.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

impl AuditLedger {
    /// Open (or create) the ledger and resume the chain from its last line. A file whose last
    /// line cannot be read starts a new chain anchored to a fresh genesis, and says so.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        repair_torn_tail(&path);
        let (seq, prev) = last_state(&path).unwrap_or((0, GENESIS.to_string()));
        Self { path, state: Mutex::new(State { seq, prev, batch: Vec::new(), head_index: 0, last_head: GENESIS_HEAD.to_string() }), signer: RwLock::new(None), lost: Mutex::new(Lost::default()) }
    }

    /// From now on tree heads are written and signed with `signer`. The lines written since the last head (if any) are picked up, so the next head
    /// covers them.
    pub fn set_head_signer(&self, signer: Arc<dyn HeadSigner>) {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let heads = heads::read_heads(&heads::heads_path(&self.path));
        let last = heads.iter().rev().find_map(|h| h.as_ref().ok()).cloned();
        let covered = last.as_ref().map(|h| h.to_seq).unwrap_or(0);
        st.head_index = last.as_ref().map(|h| h.index).unwrap_or(0);
        st.last_head = last.map(|h| h.hash).unwrap_or_else(|| GENESIS_HEAD.to_string());
        st.batch = kept_lines(&self.path).into_iter().filter(|l| l.seq > covered).map(|l| (l.seq, l.hash)).collect();
        *self.signer.write().unwrap_or_else(PoisonError::into_inner) = Some(signer);
    }

    /// Writes a head over the lines since the last one, even if there are fewer than [`HEAD_EVERY`]: so that what is shown to somebody else is covered.
    /// `Ok(None)` when there is nothing new or no signer.
    pub fn seal(&self) -> Result<Option<Head>, String> {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        self.seal_locked(&mut st)
    }

    fn seal_locked(&self, st: &mut State) -> Result<Option<Head>, String> {
        let Some(signer) = self.signer.read().unwrap_or_else(PoisonError::into_inner).clone() else { return Ok(None) };
        let Some(head) = heads::make_head(signer.as_ref(), st.head_index + 1, &st.last_head, &st.batch, now_ms()) else { return Ok(None) };
        let text = serde_json::to_string(&head).map_err(|e| e.to_string())?;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(heads::heads_path(&self.path)).map_err(|e| format!("open the heads file: {e}"))?;
        writeln!(f, "{text}").map_err(|e| format!("write the heads file: {e}"))?;
        f.sync_data().map_err(|e| format!("sync the heads file: {e}"))?;
        st.head_index = head.index;
        st.last_head = head.hash.clone();
        st.batch.clear();
        Ok(Some(head))
    }

    /// A background thread that seals the ledger every `interval` and once more when it is stopped (card R6-12). Before it, a head was written only every
    /// [`HEAD_EVERY`] lines and when somebody asked for a proof, so the newest lines, and every line of a quiet hour, had no signature over them and could be rewritten without
    /// anything showing; now the unsigned tail is never older than `interval`. A crash or a power cut can still leave the last interval unsigned.
    pub fn spawn_sealer(self: &Arc<Self>, interval: Duration) -> Sealer {
        let (stop, rx) = mpsc::channel::<()>();
        let ledger = Arc::clone(self);
        let thread = std::thread::Builder::new()
            .name("audit-sealer".into())
            .spawn(move || {
                while let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(interval) {
                    if let Err(e) = ledger.seal() {
                        warn!("audit tree head not written: {e}");
                    }
                }
            })
            .map_err(|e| warn!("the audit sealer could not start: {e}"))
            .ok();
        Sealer { stop, thread, ledger: Arc::clone(self) }
    }

    /// Everything that can be checked about the ledger without a person's help: the chain of every kept file, the chain of the signed heads, and the heads against the lines
    /// (the root of each head's lines is worked out again). `public_keys` are the keys whose signatures are accepted. The chain alone cannot tell a ledger that was rewritten
    /// consistently; the heads can, for every line that was sealed (card R6-12).
    pub fn check(&self, public_keys: &[Vec<u8>]) -> LedgerCheck {
        let mut c = LedgerCheck::default();
        match verify_chain(&self.path) {
            Ok(n) => c.lines = n,
            // No file, or one that cannot be read at all: a ledger that has not been written yet.
            Err(e) if e.line == 0 => {}
            Err(e) => {
                c.broken_at_line = Some(e.line);
                c.problems.push(e.to_string());
            }
        }
        let head_list = heads::read_heads(&heads::heads_path(&self.path));
        c.heads = head_list.len();
        let lines: BTreeMap<u64, String> = kept_lines(&self.path).into_iter().map(|l| (l.seq, l.hash)).collect();
        c.problems.extend(heads::verify_chain(&head_list, public_keys).into_iter().map(|p| format!("the signed heads: {p}")));
        let against = heads::verify_against_lines(&head_list, &lines);
        c.covered_lines = against.covered_lines;
        c.unchecked_heads = against.unchecked_heads;
        c.problems.extend(against.problems.into_iter().map(|p| format!("the signed heads and the lines: {p}")));
        let last_signed = head_list.iter().rev().find_map(|h| h.as_ref().ok()).map(|h| h.to_seq).unwrap_or(0);
        c.unsealed_lines = lines.keys().filter(|seq| **seq > last_signed).count() as u64;
        c.notices = self.evidence_notices();
        c
    }

    /// The newest lines of the live file, newest first, without their hashes. Read from the end of the file in growing blocks, so a ledger of many lines is not read whole to show
    /// the last few (card R6-12); `run` keeps only the lines of that agent run (the scan then goes back as far as it takes to find `limit` of them, or to the start of the file).
    pub fn recent(&self, limit: usize, run: Option<&str>) -> Vec<Value> {
        let Ok(mut f) = std::fs::File::open(&self.path) else { return Vec::new() };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let mut window: u64 = 256 * 1024;
        loop {
            let start = len.saturating_sub(window);
            let mut buf = Vec::new();
            if f.seek(SeekFrom::Start(start)).is_err() || (&mut f).take(len - start).read_to_end(&mut buf).is_err() {
                return Vec::new();
            }
            let text = String::from_utf8_lossy(&buf);
            let mut lines: Vec<&str> = text.lines().collect();
            if start > 0 && !lines.is_empty() {
                // The block begins in the middle of a line.
                lines.remove(0);
            }
            let rows: Vec<Value> = lines
                .iter()
                .rev()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .filter(|v| run.is_none_or(|r| v["agent_run_id"].as_str() == Some(r)))
                .take(limit)
                .map(|mut v| {
                    if let Some(o) = v.as_object_mut() {
                        o.remove("prev");
                        o.remove("hash");
                    }
                    v
                })
                .collect();
            if rows.len() >= limit || start == 0 {
                return rows;
            }
            window = window.saturating_mul(4);
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What the person should know about records that are gone, in words: that log rotation deleted the oldest file since Aqua started (with the last record number that was in
    /// it), or that the oldest record kept is not the first (an earlier run rotated the older ones away). Empty when nothing was lost. The chain still verifies either way; what a run
    /// did in the records that are gone can no longer be checked against the ledger (card OA1-04, SE-19).
    pub fn evidence_notices(&self) -> Vec<String> {
        let lost = *self.lost.lock().unwrap_or_else(PoisonError::into_inner);
        if lost.files > 0 {
            return vec![format!(
                "Log rotation deleted the oldest audit file {} time(s) since Aqua started: records up to number {} are gone for good. Only the newest {KEEP_ROTATED} rotated files and the live one are kept.",
                lost.files, lost.to_seq
            )];
        }
        match self.oldest_kept() {
            Some((seq, _)) if seq > 1 => vec![format!("Records 1 to {} were removed by log rotation in an earlier run. Only the newest {KEEP_ROTATED} rotated files and the live one are kept.", seq - 1)],
            _ => Vec::new(),
        }
    }

    /// The oldest line that is still kept: its sequence number and when it was written (milliseconds since 1970). A first line that is not number 1 means that older files
    /// were rotated away: what a run did before that time is no longer in the ledger and cannot be checked against it.
    pub fn oldest_kept(&self) -> Option<(u64, u64)> {
        kept_lines(&self.path).first().map(|l| (l.seq, l.ts_unix_ms))
    }

    /// Append one event. Returns the line's hash, or an error message when it could not be
    /// written (the caller decides whether that stops the call).
    pub fn append(&self, event: &AuditEvent) -> Result<String, String> {
        let event = &event.bounded();
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() > ROTATE_BYTES) {
            // Keep the last few files, not only one: the oldest goes, the others move up, the live file becomes `.1`.
            let oldest = self.rotated_path(KEEP_ROTATED);
            if oldest.exists() {
                // The oldest evidence leaves the kept history here. The chain still verifies (it starts at the next file).
                warn!("audit log rotation: the oldest file {} is being deleted (only {} are kept)", oldest.display(), KEEP_ROTATED);
                // And it is said where the person looks: `check` reports it (the verify route shows it), not only the log.
                let mut lost = self.lost.lock().unwrap_or_else(PoisonError::into_inner);
                lost.files += 1;
                lost.to_seq = last_seq_in(&oldest).unwrap_or(lost.to_seq);
            }
            let _ = std::fs::remove_file(oldest);
            for i in (1..KEEP_ROTATED).rev() {
                let from = self.rotated_path(i);
                if from.exists() {
                    let _ = std::fs::rename(&from, self.rotated_path(i + 1));
                }
            }
            std::fs::rename(&self.path, self.rotated_path(1)).map_err(|e| format!("rotate audit log: {e}"))?;
        }
        let seq = st.seq + 1;
        let ts = now_ms();
        let hash = digest(&st.prev, seq, ts, event);
        let line = Line { seq, ts_unix_ms: ts, prev: st.prev.clone(), event: event.clone(), hash: hash.clone() };
        let text = serde_json::to_string(&line).map_err(|e| e.to_string())?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("open audit log: {e}"))?;
        writeln!(f, "{text}").map_err(|e| format!("write audit log: {e}"))?;
        // The record is the point of the ledger: make it survive a crash or power loss right after.
        f.sync_data().map_err(|e| format!("sync audit log: {e}"))?;
        st.seq = seq;
        st.prev = hash.clone();
        // With a signer, a head is written every `HEAD_EVERY` lines. A head that cannot be written does not fail the call: the lines wait for the next one.
        if self.signer.read().unwrap_or_else(PoisonError::into_inner).is_some() {
            st.batch.push((seq, hash.clone()));
            if st.batch.len() >= HEAD_EVERY {
                if let Err(e) = self.seal_locked(&mut st) {
                    warn!("audit tree head not written: {e}");
                }
            }
        }
        Ok(hash)
    }

    fn rotated_path(&self, n: usize) -> PathBuf {
        self.path.with_extension(format!("{n}.jsonl"))
    }

    /// Check every line of the current file. Returns the number of lines verified.
    pub fn verify(&self) -> Result<u64, ChainError> {
        verify_file(&self.path)
    }

    /// Check the whole kept history: every rotated file and the live one, oldest first. Each file must continue exactly
    /// where the one before it ended, so a file removed from the middle, or the start of the log cut off, is found. The
    /// oldest file that is kept may start anywhere (older files were rotated away on purpose), unless it starts at
    /// sequence 1, when it must start from the genesis hash.
    pub fn verify_all(&self) -> Result<u64, ChainError> {
        verify_chain(&self.path)
    }
}

/// The thread that seals a ledger on a timer (see [`AuditLedger::spawn_sealer`]). Stopping it, or dropping it, seals once more.
pub struct Sealer {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
    ledger: Arc<AuditLedger>,
}

impl Sealer {
    /// Stop the timer and seal what was written since the last head. Safe to call more than once.
    pub fn stop(&mut self) {
        let _ = self.stop.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Err(e) = self.ledger.seal() {
            warn!("audit tree head not written at shutdown: {e}");
        }
    }
}

impl Drop for Sealer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What [`AuditLedger::check`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LedgerCheck {
    /// Lines in the kept chain (0: nothing written yet).
    pub lines: u64,
    /// Signed heads, and how many lines lie under a head whose root was worked out again from the lines and matched.
    pub heads: usize,
    pub covered_lines: u64,
    /// Heads whose lines were rotated away on purpose: their signature and place in the chain are checked, their root cannot be.
    pub unchecked_heads: usize,
    /// Lines after the last signed head: chained, not yet signed.
    pub unsealed_lines: u64,
    /// The line at which the chain broke, when it did.
    pub broken_at_line: Option<usize>,
    /// What is wrong, in words. Empty when everything that can be checked is right.
    pub problems: Vec<String>,
    /// What is not wrong but the person should know: records that log rotation deleted (see [`AuditLedger::evidence_notices`]).
    pub notices: Vec<String>,
}

/// [`AuditLedger::verify_all`] for a ledger file that is not open (the doctor checks it read-only).
pub fn verify_chain(path: &Path) -> Result<u64, ChainError> {
    let rotated = |n: usize| path.with_extension(format!("{n}.jsonl"));
    let mut files: Vec<PathBuf> = (1..=KEEP_ROTATED).rev().map(rotated).filter(|p| p.exists()).collect();
    files.push(path.to_path_buf());
    let mut total = 0u64;
    let mut expected_prev: Option<String> = None;
    for f in files {
        if !f.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&f).map_err(|e| ChainError { line: 0, reason: format!("cannot read {}: {e}", f.display()) })?;
        let (n, first, last) = verify_text(&text, expected_prev.as_deref())?;
        if let Some(first) = first {
            if expected_prev.is_none() && first.seq == 1 && first.prev != GENESIS {
                return Err(ChainError { line: 1, reason: format!("{} starts at sequence 1 but not from the genesis hash", f.display()) });
            }
        }
        total += n;
        if last.is_some() {
            expected_prev = last;
        }
    }
    Ok(total)
}

/// How many rotated files are kept next to the live one.
const KEEP_ROTATED: usize = 5;

/// The sequence number of the last whole line of a ledger file, or `None` when it has none that can be read.
fn last_seq_in(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines().rev().find_map(|l| serde_json::from_str::<Line>(l).ok()).map(|l| l.seq)
}

/// The lines of the ledger that are still kept (the rotated files and the live one), oldest first. A line that cannot be read is left out: `verify_all`
/// is what says so.
pub(crate) fn kept_lines(path: &Path) -> Vec<Line> {
    let rotated = |n: usize| path.with_extension(format!("{n}.jsonl"));
    let mut files: Vec<PathBuf> = (1..=KEEP_ROTATED).rev().map(rotated).filter(|p| p.exists()).collect();
    files.push(path.to_path_buf());
    let mut out = Vec::new();
    for f in files {
        if let Ok(text) = std::fs::read_to_string(&f) {
            out.extend(text.lines().filter_map(|l| serde_json::from_str::<Line>(l).ok()));
        }
    }
    out
}

/// A crash in the middle of a write leaves half a line at the end. Keep that fragment as
/// evidence in `<name>.torn.jsonl`, cut it off, and let the chain continue from the last whole
/// line; left in place it would glue itself to the next record and break the chain.
fn repair_torn_tail(path: &Path) {
    const NL: u8 = 10;
    let Ok(bytes) = std::fs::read(path) else { return };
    if bytes.is_empty() || bytes.last() == Some(&NL) {
        return;
    }
    let cut = bytes.iter().rposition(|b| *b == NL).map(|i| i + 1).unwrap_or(0);
    let torn = &bytes[cut..];
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path.with_extension("torn.jsonl")) {
        let _ = f.write_all(torn);
        let _ = f.write_all(&[NL]);
    }
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = f.set_len(cut as u64);
    }
}

fn last_state(path: &Path) -> Option<(u64, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let last = text.lines().rev().find(|l| !l.trim().is_empty())?;
    let line: Line = serde_json::from_str(last).ok()?;
    Some((line.seq, line.hash))
}

static DIRECT_LEDGER: std::sync::OnceLock<std::sync::Arc<AuditLedger>> = std::sync::OnceLock::new();
static DIRECT_RUN: std::sync::OnceLock<fn() -> String> = std::sync::OnceLock::new();

/// Say which agent run is on this thread, for the direct lines (card R6-12): an action that goes round the host pipeline (opening an app or a page) was written without a run,
/// so the proof of a run never showed it. The function gives the run id of the calling thread, or an empty text outside a run. Returns false when one was already set.
pub fn set_direct_run_source(source: fn() -> String) -> bool {
    DIRECT_RUN.set(source).is_ok()
}

/// The event of a direct action: no policy decision, so no approval note; the run, when the caller is inside one.
fn direct_event(n: u64, run: &str, caller: &str, tool: &str, target: &str, ok: bool, duration_ms: u64) -> AuditEvent {
    AuditEvent {
        request_id: format!("direct-{n}"),
        session_id: "direct".into(),
        agent_run_id: run.into(),
        caller: caller.into(),
        tool: tool.into(),
        args: serde_json::json!({ "target": target }),
        decision: "direct".into(),
        classes: Vec::new(),
        ok,
        error_code: None,
        duration_ms,
        shadow: None,
        approval: None,
    }
}
static DIRECT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Let code that acts outside the tool-host pipeline write to the same ledger: a spoken "open Notepad", "open the installer",
/// opening a web page in the person's browser. Until this is called (a test, a tool that runs without a host) such actions are
/// not recorded. Returns false when a ledger was already installed.
pub fn install_direct_ledger(ledger: std::sync::Arc<AuditLedger>) -> bool {
    DIRECT_LEDGER.set(ledger).is_ok()
}

/// Record something done outside the host pipeline. The line has `decision: "direct"` and is part of the same hash chain, so
/// `/tools/audit/verify` covers it. `target` is what was opened or started (a path, an app name, a host name; never a URL with
/// a query or a secret), `caller` who asked (`voice`, `agent`, `ui`). Never fails the action: a ledger that cannot be
/// written is logged. Returns whether a line was written.
pub fn record_direct(caller: &str, tool: &str, target: &str, ok: bool, duration_ms: u64) -> bool {
    let Some(ledger) = DIRECT_LEDGER.get() else { return false };
    let n = DIRECT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let run = DIRECT_RUN.get().map(|source| source()).unwrap_or_default();
    let event = direct_event(n, &run, caller, tool, target, ok, duration_ms);
    match ledger.append(&event) {
        Ok(_) => true,
        Err(e) => {
            warn!("audit write failed for {tool} (direct): {e}");
            false
        }
    }
}

/// Verify a ledger file. The first line may point at any `prev` (a rotated file continues the
/// chain of its predecessor); every later line must point at the hash of the line before it.
pub fn verify_file(path: &Path) -> Result<u64, ChainError> {
    let text = std::fs::read_to_string(path).map_err(|e| ChainError { line: 0, reason: format!("cannot read: {e}") })?;
    verify_text(&text, None).map(|(n, _, _)| n)
}

/// Verify the lines of one file. `first_prev`, when given, is the hash the first line must continue from. Returns the
/// number of lines, the first line (its sequence number and `prev`) and the last line's hash.
fn verify_text(text: &str, first_prev: Option<&str>) -> Result<(u64, Option<FirstLine>, Option<String>), ChainError> {
    let mut prev: Option<String> = first_prev.map(str::to_string);
    let mut prev_seq: Option<u64> = None;
    let mut first: Option<FirstLine> = None;
    let mut n = 0u64;
    for (i, raw) in text.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        let no = i + 1;
        let line: Line = serde_json::from_str(raw).map_err(|e| ChainError { line: no, reason: format!("not a valid record: {e}") })?;
        if first.is_none() {
            first = Some(FirstLine { seq: line.seq, prev: line.prev.clone() });
        }
        if let Some(p) = &prev {
            if &line.prev != p {
                return Err(ChainError { line: no, reason: "previous hash does not match (line removed, reordered or edited)".into() });
            }
        }
        if let Some(ps) = prev_seq {
            if line.seq != ps + 1 {
                return Err(ChainError { line: no, reason: format!("sequence jumps from {ps} to {}", line.seq) });
            }
        }
        let expect = digest(&line.prev, line.seq, line.ts_unix_ms, &line.event);
        if expect != line.hash {
            return Err(ChainError { line: no, reason: "content does not match its hash (line edited)".into() });
        }
        prev = Some(line.hash);
        prev_seq = Some(line.seq);
        n += 1;
    }
    Ok((n, first, if n > 0 { prev } else { None }))
}

struct FirstLine {
    seq: u64,
    prev: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Opening an app or a page by voice goes round the host pipeline; it must still leave a line in the same verifiable chain.
    #[test]
    fn actions_outside_the_pipeline_write_direct_lines_into_the_same_chain() {
        let dir = std::env::temp_dir().join(format!("aqua_direct_ledger_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("host.audit.jsonl");
        assert!(!record_direct("voice", "app.launch", "Notepad", true, 5), "nothing is written before a ledger is installed");
        let ledger = std::sync::Arc::new(AuditLedger::open(&path));
        assert!(install_direct_ledger(ledger.clone()));
        assert!(!install_direct_ledger(ledger.clone()), "only one ledger is installed");
        assert!(record_direct("voice", "app.launch", "Notepad", true, 5));
        assert!(record_direct("agent", "file.open", r"C:\Users\x\Downloads\setup.exe", false, 1));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(text.contains(r#""decision":"direct""#) && text.contains("app.launch") && text.contains("Notepad") && text.contains("setup.exe"), "{text}");
        assert_eq!(ledger.verify().unwrap(), 2, "the chain verifies with the direct lines in it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn large_text_arguments_are_kept_as_a_beginning_plus_length_and_hash() {
        let big = "x".repeat(50_000);
        let shrunk = shrink_args(&json!({"path": "a.txt", "content": big, "n": 3, "list": ["short", big]}));
        assert_eq!(shrunk["path"], "a.txt");
        assert_eq!(shrunk["n"], 3);
        let c = shrunk["content"].as_str().unwrap();
        assert!(c.len() < 400 && c.starts_with(&"x".repeat(200)) && c.contains("50000 chars") && c.contains("sha256 "), "{c}");
        assert_eq!(shrunk["list"][0], "short");
        assert!(shrunk["list"][1].as_str().unwrap().contains("50000 chars"));
        // the same text always gives the same hash, a different text a different one
        assert_eq!(shrink_args(&json!({"c": big})), shrink_args(&json!({"c": big})));
        assert_ne!(shrink_args(&json!({"c": big})), shrink_args(&json!({"c": format!("{big}y")})));
        // text just under the limit is untouched, multi-byte text is counted in characters
        let edge = "a".repeat(MAX_ARG_FIELD_CHARS);
        assert_eq!(shrink_args(&json!({"c": edge})), json!({"c": edge}));
        let wide = "é".repeat(MAX_ARG_FIELD_CHARS + 1);
        assert!(shrink_args(&json!({"c": wide}))["c"].as_str().unwrap().contains("chars, sha256"));
    }

    #[test]
    fn a_record_that_is_still_too_large_is_replaced_by_its_size_and_hash() {
        let many: Vec<Value> = (0..400).map(|i| json!(format!("{i}{}", "z".repeat(100)))).collect();
        let s = shrink_args(&json!({"items": many}));
        assert_eq!(s["truncated"], true);
        assert!(s["bytes"].as_u64().unwrap() as usize > MAX_ARGS_BYTES && s["sha256"].as_str().unwrap().len() == 64, "{s}");
    }

    #[test]
    fn a_long_name_is_cut_to_its_beginning_plus_length_and_hash_and_cutting_again_changes_nothing() {
        let long = "x".repeat(5000);
        let once = clip_field(&long, MAX_ID_CHARS);
        assert!(once.chars().count() <= MAX_ID_CHARS && once.contains("5000 chars, sha256 "), "{once}");
        assert_eq!(clip_field(&once, MAX_ID_CHARS), once, "a text that was cut already is within the limit");
        assert_ne!(clip_field(&format!("{long}a"), MAX_ID_CHARS), clip_field(&format!("{long}b"), MAX_ID_CHARS), "two long names stay two names");
        assert_eq!(clip_field("fs.read", MAX_ID_CHARS), "fs.read");
        let edge = "a".repeat(MAX_ID_CHARS);
        assert_eq!(clip_field(&edge, MAX_ID_CHARS), edge, "exactly the limit is kept");
        let wide = "\u{e9}".repeat(MAX_ID_CHARS);
        assert_eq!(clip_field(&wide, MAX_ID_CHARS), wide, "the limit counts characters, not bytes");
        assert!(clip_field(&format!("{wide}\u{e9}"), MAX_ID_CHARS).contains("201 chars, sha256"));
        let ordinary = event("fs.read", true);
        assert_eq!(ordinary.bounded(), ordinary, "an ordinary event is recorded as it is");
    }

    /// Found in a review of the gateway: only `args` was capped, so a refused call ("unknown tool") kept the tool name and the session id
    /// as sent, up to the 2 MB body limit, and five such requests rotated every older record out of the kept files.
    #[test]
    fn a_call_cannot_make_one_line_of_the_log_large() {
        let l = AuditLedger::open(temp("bounded"));
        let mut e = event(&"t".repeat(2 * 1024 * 1024), false);
        e.session_id = "s".repeat(1_000_000);
        e.request_id = "r".repeat(300);
        e.agent_run_id = "\u{e9}".repeat(5000);
        e.caller = "c".repeat(9000);
        e.decision = "d".repeat(10_000);
        e.classes = (0..500).map(|i| format!("CLASS{i}{}", "x".repeat(100))).collect();
        e.error_code = Some("e".repeat(10_000));
        e.args = json!({"junk": "j".repeat(40_000)});
        l.append(&e).unwrap();
        let size = std::fs::metadata(l.path()).unwrap().len();
        assert!(size < 6 * 1024, "one refused call made a line of {size} bytes");
        let text = std::fs::read_to_string(l.path()).unwrap();
        assert!(text.contains("2097152 chars, sha256 ") && text.contains("1000000 chars, sha256 "), "the lengths are kept as evidence");
        assert_eq!(l.verify().unwrap(), 1, "the line as written still verifies");
        // Flooding with such calls no longer rotates anything: ten of them are far below the 10 MB at which the log rotates.
        for _ in 0..10 {
            l.append(&e).unwrap();
        }
        assert!(!l.path().with_extension("1.jsonl").exists());
        assert_eq!(l.verify_all().unwrap(), 11);
    }

    fn event(tool: &str, ok: bool) -> AuditEvent {
        AuditEvent {
            request_id: "req_1".into(),
            session_id: "ses_1".into(),
            agent_run_id: "run_1".into(),
            caller: "agent".into(),
            tool: tool.into(),
            args: json!({"path": "a.txt"}),
            decision: "auto".into(),
            classes: vec!["READ".into()],
            ok,
            error_code: None,
            duration_ms: 3,
            shadow: None,
            approval: None,
        }
    }

    thread_local! {
        /// The folders the running test made. They are removed when its thread ends: the rotation tests write 10 MB files, and each run of
        /// the suite used to leave about 900 MB of them in the temp folder.
        static MADE: std::cell::RefCell<Vec<RemoveFolder>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    struct RemoveFolder(PathBuf);

    impl Drop for RemoveFolder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_audit_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        MADE.with(|made| made.borrow_mut().push(RemoveFolder(d.clone())));
        d.join("audit.jsonl")
    }

    /// A ledger with a signer, in its own folder.
    fn signed(name: &str) -> (Arc<AuditLedger>, Arc<crate::heads::test_support::TestSigner>) {
        let l = Arc::new(AuditLedger::open(temp(name)));
        let signer = Arc::new(crate::heads::test_support::TestSigner::new());
        l.set_head_signer(signer.clone());
        (l, signer)
    }

    fn heads_of(l: &AuditLedger) -> Vec<Head> {
        // A head that is being written as this is read is half a line: left out, and there next time.
        heads::read_heads(&heads::heads_path(l.path())).into_iter().filter_map(Result::ok).collect()
    }

    /// Card R6-12: a head was written only every 64 lines and when a proof was asked for, so the newest lines had no signature over them.
    #[test]
    fn the_sealer_signs_the_lines_within_its_interval_and_once_more_when_it_is_stopped() {
        let (l, _) = signed("sealer");
        let mut sealer = l.spawn_sealer(Duration::from_millis(40));
        for i in 0..3 {
            l.append(&event(&format!("fs.read{i}"), true)).unwrap();
        }
        // No help from anybody: a head over the three lines appears by itself, well within a second.
        let started = std::time::Instant::now();
        while heads_of(&l).last().is_none_or(|h| h.to_seq < 3) {
            assert!(started.elapsed() < Duration::from_secs(5), "no head within five seconds: {:?}", heads_of(&l));
            std::thread::sleep(Duration::from_millis(20));
        }
        sealer.stop();
        // With an interval that never comes, only the stop seals: lines written after the last head are covered by the head that stopping writes.
        let mut slow = l.spawn_sealer(Duration::from_secs(3600));
        l.append(&event("fs.read3", true)).unwrap();
        l.append(&event("fs.read4", true)).unwrap();
        assert_eq!(heads_of(&l).last().unwrap().to_seq, 3, "nothing has sealed the two new lines yet");
        slow.stop();
        assert_eq!(heads_of(&l).last().unwrap().to_seq, 5, "stopping sealed them");
        slow.stop(); // twice is harmless
        assert_eq!(l.check(&[]).unsealed_lines, 0);
    }

    /// Card R6-12: the verify route and the start-up check ran the chain and not the heads, and the chain cannot tell a ledger that was rewritten consistently.
    #[test]
    fn a_check_runs_the_chain_and_the_heads_and_says_how_much_is_not_yet_signed() {
        let (l, signer) = signed("check");
        for i in 0..70 {
            l.append(&event(&format!("fs.read{i}"), true)).unwrap();
        }
        let keys = [signer.public_key()];
        let c = l.check(&keys);
        assert_eq!((c.lines, c.heads, c.covered_lines, c.unsealed_lines), (70, 1, 64, 6), "{c:?}");
        assert!(c.problems.is_empty() && c.broken_at_line.is_none(), "{c:?}");
        l.seal().unwrap();
        let c = l.check(&keys);
        assert_eq!((c.heads, c.covered_lines, c.unsealed_lines), (2, 70, 0), "{c:?}");
        // The wrong key: the heads are signed by a key that is not known.
        let stranger = crate::heads::test_support::TestSigner::new();
        assert!(l.check(&[stranger.public_key()]).problems.iter().any(|p| p.contains("which is not one that is known")));
        // The end of the ledger cut off (the lines below are a valid chain, as far as they go): the chain alone cannot see it, the heads do.
        let text = std::fs::read_to_string(l.path()).unwrap();
        let kept: Vec<&str> = text.lines().take(60).collect();
        std::fs::write(l.path(), format!("{}\n", kept.join("\n"))).unwrap();
        assert_eq!(l.verify_all().unwrap(), 60, "the chain alone is satisfied");
        let cut = l.check(&keys);
        assert!(!cut.problems.is_empty(), "the heads say that signed lines are gone: {cut:?}");
        // A ledger nothing was written to is not a problem.
        let empty = AuditLedger::open(temp("check_empty"));
        assert_eq!(empty.check(&[]), LedgerCheck::default());
    }

    /// Card R6-12: a line did not say whether an approval was the person's answer or a grant from earlier, or why it was asked.
    #[test]
    fn a_line_with_an_approval_is_covered_by_the_hash_and_a_line_without_one_still_verifies() {
        let p = temp("approval");
        let l = AuditLedger::open(&p);
        l.append(&event("fs.read", true)).unwrap(); // a line as they were written before the note existed
        let mut with = event("fs.write", true);
        with.approval = Some(ApprovalNote { kind: approval_kind("approved_session").into(), by: "policy".into(), reason: "granted earlier in this session".into() });
        l.append(&with).unwrap();
        assert_eq!(l.verify_all().unwrap(), 2);
        let text = std::fs::read_to_string(&p).unwrap();
        let (first, second) = text.split_once('\n').unwrap();
        assert!(!first.contains("approval"), "a line without a note is written as it always was: {first}");
        assert!(second.contains(r#""approval":{"kind":"session","by":"policy","reason":"granted earlier in this session"}"#), "{second}");
        // Changing the kind of an approval after the fact breaks the chain.
        std::fs::write(&p, text.replace(r#""kind":"session""#, r#""kind":"once""#)).unwrap();
        assert!(l.verify_all().is_err(), "a changed approval is a changed line");
        for (decision, kind) in [("auto", "policy"), ("approved", "once"), ("approved_always", "always"), ("approved_session", "session"), ("preapproved", "preapproved"), ("denied", "refused"), ("timeout", "refused"), ("cancelled", "refused")] {
            assert_eq!(approval_kind(decision), kind, "{decision}");
        }
        // Long words are cut like every other text of a line.
        let long = AuditEvent { approval: Some(ApprovalNote { kind: "k".repeat(999), by: "b".repeat(999), reason: "r".repeat(99_999) }), ..event("fs.write", true) };
        let b = long.bounded().approval.unwrap();
        // (A text that is cut keeps its start and says how long it was and what its hash is, which is about a hundred characters more.)
        assert!(b.kind.chars().count() < 200 && b.by.chars().count() < 200 && b.reason.chars().count() < 400, "{b:?}");
        assert!(b.kind.contains("999 chars") && b.reason.contains("99999 chars"), "the lengths are kept as evidence: {b:?}");
    }

    /// Card R6-12: an action that went round the host (opening an app or a page) was written with no run, so the proof of a run never showed it.
    #[test]
    fn a_direct_line_carries_the_run_it_was_made_in() {
        let e = direct_event(7, "run_9", "voice", "app.open", "notepad", true, 12);
        assert_eq!((e.agent_run_id.as_str(), e.decision.as_str(), e.session_id.as_str()), ("run_9", "direct", "direct"));
        assert!(e.approval.is_none(), "no policy decided it");
        let p = temp("direct");
        let l = AuditLedger::open(&p);
        l.append(&e).unwrap();
        l.append(&direct_event(8, "", "voice", "app.open", "calc", true, 3)).unwrap();
        assert_eq!(l.recent(10, Some("run_9")).len(), 1, "the run's lines include it");
        assert_eq!(l.recent(10, None).len(), 2);
    }

    /// Card R6-12: `/tools/audit/recent` read the whole file for every request.
    #[test]
    fn the_newest_lines_are_read_from_the_end_and_can_be_limited_to_one_run() {
        let p = temp("recent");
        let l = AuditLedger::open(&p);
        // The first three lines belong to run "old"; after them come 320 lines of about a kilobyte each, more than the first block that is read.
        for i in 0..3 {
            let mut e = event(&format!("old.tool{i}"), true);
            e.agent_run_id = "old".into();
            l.append(&e).unwrap();
        }
        for i in 0..320 {
            let mut e = event(&format!("new.tool{i}"), true);
            e.agent_run_id = "new".into();
            e.args = json!({"pad": "x".repeat(900)});
            l.append(&e).unwrap();
        }
        assert!(std::fs::metadata(&p).unwrap().len() > 300 * 1024);
        let newest = l.recent(5, None);
        let names: Vec<&str> = newest.iter().map(|r| r["tool"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["new.tool319", "new.tool318", "new.tool317", "new.tool316", "new.tool315"], "newest first");
        assert!(newest.iter().all(|r| r.get("hash").is_none() && r.get("prev").is_none()), "the hash fields are left out");
        // The old run is found by going back past the first block.
        let old = l.recent(10, Some("old"));
        assert_eq!(old.iter().map(|r| r["tool"].as_str().unwrap()).collect::<Vec<_>>(), vec!["old.tool2", "old.tool1", "old.tool0"]);
        assert!(l.recent(10, Some("nobody")).is_empty());
        assert_eq!(l.recent(1000, None).len(), 323, "a limit above the ledger gives all of it, each line whole");
    }

    #[test]
    fn a_clean_chain_verifies_and_survives_reopening() {
        let p = temp("clean");
        let l = AuditLedger::open(&p);
        for i in 0..5 {
            l.append(&event(&format!("fs.read{i}"), true)).unwrap();
        }
        assert_eq!(l.verify().unwrap(), 5);
        let again = AuditLedger::open(&p);
        again.append(&event("fs.list", true)).unwrap();
        assert_eq!(again.verify().unwrap(), 6);
    }

    /// Append `n` events, forcing a rotation before each of the last `rotations` ones.
    fn ledger_with_rotations(name: &str, rotations: usize) -> (AuditLedger, PathBuf) {
        let p = temp(name);
        let l = AuditLedger::open(&p);
        l.append(&event("fs.read0", true)).unwrap();
        for i in 0..rotations {
            // Pad the live file past the limit; the next append rotates it and starts a new file.
            let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
            f.write_all(&vec![b' '; ROTATE_BYTES as usize + 1]).unwrap();
            drop(f);
            l.append(&event(&format!("fs.read{}", i + 1), true)).unwrap();
        }
        (l, p)
    }

    /// Card OA1-04 (SE-19): a rotation that deletes the oldest file used to be a line in the log and nothing else; the check now says it, and says it again after a restart.
    #[test]
    fn a_rotation_that_deletes_evidence_is_reported_by_the_check_and_one_that_does_not_is_not() {
        let (l, _) = ledger_with_rotations("notice_none", KEEP_ROTATED);
        assert!(l.evidence_notices().is_empty(), "every file is still kept: nothing is gone");
        assert!(l.check(&[]).notices.is_empty());
        drop(l);
        let (l, p2) = ledger_with_rotations("notice_lost", KEEP_ROTATED + 1);
        let notices = l.check(&[]).notices;
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("deleted the oldest audit file 1 time(s)") && notices[0].contains("up to number 1 "), "{}", notices[0]);
        // A new run of Aqua opens the same files: the loss is still there to be told, from the first record that remains.
        let reopened = AuditLedger::open(&p2);
        let after_restart = reopened.check(&[]).notices;
        assert_eq!(after_restart.len(), 1, "{after_restart:?}");
        assert!(after_restart[0].contains("Records 1 to 1 were removed"), "{}", after_restart[0]);
        assert!(reopened.verify_all().is_ok(), "the chain verifies; the notice is not a problem");
        assert!(l.check(&[]).problems.is_empty());
    }

    #[test]
    fn the_oldest_line_kept_says_whether_older_files_were_rotated_away() {
        let p = temp("oldest_kept");
        let l = AuditLedger::open(&p);
        assert_eq!(l.oldest_kept(), None, "nothing written yet");
        l.append(&event("fs.read0", true)).unwrap();
        l.append(&event("fs.read1", true)).unwrap();
        assert_eq!(l.oldest_kept().map(|(seq, _)| seq), Some(1), "the ledger starts at its first line: nothing was lost");
        // One more rotation than files are kept: the file that held line 1 is gone, and the oldest line that remains is line 2.
        let (rotated, _) = ledger_with_rotations("oldest_kept_rotated", KEEP_ROTATED + 1);
        assert_eq!(rotated.oldest_kept().map(|(seq, _)| seq), Some(2));
    }

    #[test]
    fn several_rotated_files_are_kept_and_the_whole_history_verifies_as_one_chain() {
        let (l, p) = ledger_with_rotations("rotations", 3);
        for n in 1..=3 {
            assert!(p.with_extension(format!("{n}.jsonl")).exists(), "rotated file {n} is kept");
        }
        assert_eq!(l.verify_all().unwrap(), 4, "one record in each of the four files, in one unbroken chain");
        assert_eq!(l.verify().unwrap(), 1, "the live file alone is one record");
    }

    #[test]
    fn a_file_removed_from_the_middle_is_found_across_files() {
        let (l, p) = ledger_with_rotations("rotations_middle", 3);
        let middle = p.with_extension("2.jsonl");
        let keep = std::fs::read(&middle).unwrap();
        std::fs::remove_file(&middle).unwrap();
        let err = l.verify_all().unwrap_err();
        assert!(err.reason.contains("previous hash"), "{}", err.reason);
        std::fs::write(&middle, keep).unwrap();
        assert_eq!(l.verify_all().unwrap(), 4, "put back, the chain is whole again");
    }

    #[test]
    fn a_cut_off_start_of_the_live_file_is_found_because_it_no_longer_continues_its_predecessor() {
        let p = temp("rotations_cut");
        let l = AuditLedger::open(&p);
        l.append(&event("fs.read0", true)).unwrap();
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(&vec![b' '; ROTATE_BYTES as usize + 1]).unwrap();
        drop(f);
        l.append(&event("fs.read1", true)).unwrap(); // first record of the new live file
        l.append(&event("fs.read2", true)).unwrap();
        assert_eq!(l.verify_all().unwrap(), 3);
        // Someone removes the live file's first record.
        let live = std::fs::read_to_string(&p).unwrap();
        let rest: Vec<&str> = live.lines().filter(|x| !x.trim().is_empty()).skip(1).collect();
        std::fs::write(&p, format!("{}\n", rest.join("\n"))).unwrap();
        assert_eq!(l.verify().unwrap(), 1, "checked alone, the remaining line looks fine (this is the old blind spot)");
        let err = l.verify_all().unwrap_err();
        assert!(err.reason.contains("previous hash"), "{}", err.reason);
    }

    #[test]
    fn a_crash_mid_write_keeps_the_chain_and_the_fragment() {
        let p = temp("torn");
        let l = AuditLedger::open(&p);
        for i in 0..3 {
            l.append(&event(&format!("fs.read{i}"), true)).unwrap();
        }
        // Half of a fourth record reached the disk.
        let full = std::fs::read_to_string(&p).unwrap();
        let half = "{\"seq\":4,\"ts_unix_ms\":17,\"pre";
        std::fs::write(&p, format!("{full}{half}")).unwrap();
        let again = AuditLedger::open(&p);
        again.append(&event("fs.list", true)).unwrap();
        assert_eq!(again.verify().unwrap(), 4, "three whole records plus the new one, one unbroken chain");
        let torn = std::fs::read_to_string(p.with_extension("torn.jsonl")).unwrap();
        assert!(torn.contains(half), "the fragment is kept as evidence");
        assert!(!std::fs::read_to_string(&p).unwrap().contains(half));
        // A file that ends cleanly is left exactly as it is.
        let clean = std::fs::read(&p).unwrap();
        let _ = AuditLedger::open(&p);
        assert_eq!(std::fs::read(&p).unwrap(), clean);
    }

    #[test]
    fn editing_any_byte_of_a_line_is_detected() {
        let p = temp("edit");
        let l = AuditLedger::open(&p);
        for _ in 0..4 {
            l.append(&event("fs.read", true)).unwrap();
        }
        let original = std::fs::read_to_string(&p).unwrap();
        // Change one character in each line in turn: every edit must be caught.
        for (i, _) in original.lines().enumerate() {
            let tampered: Vec<String> = original
                .lines()
                .enumerate()
                .map(|(j, s)| if j == i { s.replacen("fs.read", "fs.wipe", 1) } else { s.to_string() })
                .collect();
            std::fs::write(&p, tampered.join("\n") + "\n").unwrap();
            let err = verify_file(&p).unwrap_err();
            assert_eq!(err.line, i + 1, "{err}");
        }
    }

    #[test]
    fn deleting_or_reordering_lines_is_detected() {
        let p = temp("del");
        let l = AuditLedger::open(&p);
        for i in 0..4 {
            l.append(&event(&format!("t.n{i}"), true)).unwrap();
        }
        let lines: Vec<String> = std::fs::read_to_string(&p).unwrap().lines().map(String::from).collect();
        std::fs::write(&p, format!("{}\n{}\n{}\n", lines[0], lines[2], lines[3])).unwrap();
        assert!(verify_file(&p).is_err());
        std::fs::write(&p, format!("{}\n{}\n{}\n{}\n", lines[0], lines[2], lines[1], lines[3])).unwrap();
        assert!(verify_file(&p).is_err());
    }

    #[test]
    fn rotation_carries_the_chain_forward() {
        let p = temp("rot");
        let l = AuditLedger::open(&p);
        l.append(&event("a.one", true)).unwrap();
        let last_hash = last_state(&p).unwrap().1;
        // Force a rotation on the next append.
        std::fs::write(&p, std::fs::read_to_string(&p).unwrap() + &" ".repeat(ROTATE_BYTES as usize + 1)).unwrap();
        l.append(&event("a.two", true)).unwrap();
        let rotated = p.with_extension("1.jsonl");
        assert!(rotated.exists());
        let first_new: Line = serde_json::from_str(std::fs::read_to_string(&p).unwrap().lines().next().unwrap()).unwrap();
        assert_eq!(first_new.prev, last_hash);
        assert_eq!(first_new.seq, 2);
        assert_eq!(l.verify().unwrap(), 1);
    }

    #[test]
    fn an_empty_or_missing_ledger_verifies_as_zero_lines() {
        let p = temp("empty");
        let l = AuditLedger::open(&p);
        assert!(l.verify().is_err()); // not created yet
        l.append(&event("x.y", false)).unwrap();
        assert_eq!(l.verify().unwrap(), 1);
    }
}

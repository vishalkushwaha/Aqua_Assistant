//! Pauses a browser action until the user allows or denies it (UI, API or voice).
//!
//! The agent runs on a blocking thread, so a request simply waits on a channel with a timeout;
//! no answer means deny.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmKind {
    /// An unknown site in `ask` mode.
    Site,
    /// An irreversible or sensitive action (send, pay, password field, upload…).
    Action,
    /// A desktop (Windows-MCP) action that needs the user's answer.
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    /// Allow and remember the site (site confirmations only).
    AllowAlways,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfirmRequest {
    pub request_id: String,
    pub kind: ConfirmKind,
    pub tool: String,
    pub reason: String,
    pub url: Option<String>,
    /// Host to remember when a site request is answered with `allow_always`.
    pub site: Option<String>,
    pub created_unix_ms: u64,
    pub expires_in_secs: u64,
    /// Tool approvals only: this answer is never remembered, so "for this session" is not offered.
    #[serde(default)]
    pub always_ask: bool,
    /// Tool approvals only: what the call can do (`WRITE`, `EXECUTE`, `NETWORK`…), for the prompt.
    #[serde(default)]
    pub classes: Vec<String>,
    /// Tool approvals only: one sentence on what could go wrong, the rule that made it ask, and what is involved (see `aqua_tool_host::explain`).
    #[serde(default)]
    pub risk: String,
    #[serde(default)]
    pub rule: String,
    #[serde(default)]
    pub data: Vec<String>,
    /// Unix milliseconds after which an answer is too late: a question nobody answered by then counts as no, and an answer that comes after it changes nothing.
    #[serde(default)]
    pub expires_unix_ms: u64,
    /// The agent run the question is about, when it is known (a tool approval asked for a run; not a question of the browser's own), so that `/browser/confirmations?run=` can
    /// show only that run's questions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Tool approvals only (card OA6-04, ED-05, SE-03, UX-07): what the call would change or send, as the host made it from the arguments the call will run with. For a file, a unified
    /// diff (or the first lines of a new file); for a request that is sent, its method, address and body. Plain text, masked, with every invisible character shown by its number, cut at 6,000
    /// characters with a line that says how much is not shown; the app shows it as text and never as markup. Absent when the call has nothing to show, and for a file of secrets it is a
    /// sentence that the content is hidden.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Tool approvals only (card OA6-05, ED-10): present only when the host would let the answer carry the run option (`scope: "run"` of `POST /api/v1/browser/confirm`), which makes an edit grant
    /// for this run: the sentence that says plainly what the choice allows (later edits of files in this folder, until the run ends). Absent when the switch `AQUA_EDIT_GRANT` is off and for every
    /// question that is not a plain file edit of a run with someone to ask, so a question is written exactly as it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_grant: Option<String>,
}

/// What the broker tells the observer of the process (card OA10-01): a question opened, and how it ended.
#[derive(Debug, Clone, Copy)]
pub enum QuestionNote<'a> {
    /// A question is open: someone is asked, and the run that asked waits.
    Opened(&'a ConfirmRequest),
    /// The question ended. `outcome` is one of `approved`, `approved_session`, `denied` (the person said no), `timeout` (nobody answered in time) or `cancelled` (the run that asked went
    /// away, or the question was taken back).
    Closed { request: &'a ConfirmRequest, outcome: &'static str },
}

type QuestionObserver = Box<dyn Fn(QuestionNote<'_>) + Send + Sync>;

static QUESTION_OBSERVER: std::sync::OnceLock<QuestionObserver> = std::sync::OnceLock::new();

/// Sets the one observer of the process: the core's run log, which makes an `approval.opened` and an `approval.closed` event of each question. It is called on the thread that asked,
/// before it waits and after it has the answer, and must not block. The first call wins; `false` when one was set already. A question that is answered at once without being put
/// (an unattended run's, a flow's pre-approved one) is not told: nobody was asked.
pub fn set_question_observer(observer: impl Fn(QuestionNote<'_>) + Send + Sync + 'static) -> bool {
    QUESTION_OBSERVER.set(Box::new(observer)).is_ok()
}

fn note_question(note: QuestionNote<'_>) {
    if let Some(observer) = QUESTION_OBSERVER.get() {
        observer(note);
    }
}

thread_local! {
    static ASKING_FOR: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// While this guard is alive, a question asked on this thread is about the given agent run (see [`ConfirmRequest::run_id`]).
pub struct AskingForRun {
    previous: Option<String>,
}

pub fn asking_for_run(run: &str) -> AskingForRun {
    AskingForRun { previous: ASKING_FOR.with(|a| a.replace((!run.is_empty()).then(|| run.to_string()))) }
}

impl Drop for AskingForRun {
    fn drop(&mut self) {
        ASKING_FOR.with(|a| *a.borrow_mut() = self.previous.take());
    }
}

fn asking_run() -> Option<String> {
    ASKING_FOR.with(|a| a.borrow().clone())
}

/// How many ended questions are remembered, so that a late answer can be told what became of its question.
const MAX_ENDED: usize = 64;

fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// How a question that is no longer open ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    Answered(Decision),
    /// Nobody answered in time (the moment, in unix milliseconds, that an answer had to come before).
    Expired(u64),
    /// The run that asked went away (it was stopped, or the caller hung up) before an answer: the question was taken back.
    Withdrawn,
}

/// What came of an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    /// The run that asked has the answer.
    Applied,
    /// Too late: the question expired and the run was told no. Nothing was done.
    Expired { at_unix_ms: u64 },
    /// The question had been answered already (with this).
    AlreadyAnswered(Decision),
    /// The run that asked was stopped: there is nothing to answer.
    Withdrawn,
    /// No such question (it may be from before a restart).
    Unknown,
}

/// What a tool approval shows besides the action (see `aqua_tool_host::ApprovalWhy`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfirmWhy {
    pub risk: String,
    pub rule: String,
    pub data: Vec<String>,
    /// What the call would change or send (see [`ConfirmRequest::preview`]).
    pub preview: Option<String>,
    /// What the run option would allow, when the host offers it (see [`ConfirmRequest::run_grant`]).
    pub run_grant: Option<String>,
}

impl From<aqua_tool_host::ApprovalWhy> for ConfirmWhy {
    fn from(why: aqua_tool_host::ApprovalWhy) -> Self {
        Self { risk: why.risk, rule: why.rule, data: why.data, preview: None, run_grant: None }
    }
}

/// The classes of a call as a question shows them (`WRITE`, `EXTERNAL_EFFECT`): the words the host's approver gives them too.
pub fn class_words(classes: &[aqua_tool_host::PermClass]) -> Vec<String> {
    classes.iter().map(|c| format!("{c:?}").to_uppercase().replace("EXTERNALEFFECT", "EXTERNAL_EFFECT")).collect()
}

/// What a question of the old routes (the browser's own, the desktop guard's, the run's "this run has read outside text") shows besides the action, built the way the host builds
/// the question it puts for a call of its own (`aqua_tool_host::explain`), so that a person is told the same things whichever route the call took (card R6-08, T6-08.8).
/// `tainted` and `private` are what the run has read and holds, where the route knows it.
pub fn explained(classes: &[aqua_tool_host::PermClass], target: &str, rule: &str, tainted: bool, private: bool) -> ConfirmWhy {
    aqua_tool_host::explain(classes, target, rule, tainted, private, false).into()
}

thread_local! {
    static UNATTENDED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// While this guard is alive, approvals requested on this thread are refused at once: nothing is
/// queued, nothing is shown, nobody is waited for, and nothing is ever granted. Background work
/// (practice runs) uses it, because there is no person at the screen to answer and a background job
/// must never be able to approve an action for itself.
pub struct UnattendedGuard {
    previous: bool,
}

pub fn unattended() -> UnattendedGuard {
    UnattendedGuard { previous: UNATTENDED.with(|u| u.replace(true)) }
}

impl Drop for UnattendedGuard {
    fn drop(&mut self) {
        UNATTENDED.with(|u| u.set(self.previous));
    }
}

pub fn is_unattended() -> bool {
    UNATTENDED.with(|u| u.get())
}

thread_local! {
    static QUESTIONS_PUT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many questions the old routes have put on **this thread** so far (card R6-08): one for every `ConfirmationBroker::request*` made here, whether a person answered it,
/// a flow had answered it already, or nobody could be asked. A run's tool calls are made on one thread, and the host's own approvals are made on others, so the difference
/// between two readings on the run's thread is the number of times the old route asked about the call in between. The one-door comparison reads it; nothing else does.
pub fn questions_put_on_this_thread() -> u64 {
    QUESTIONS_PUT.with(|q| q.get())
}

thread_local! {
    static ABANDON: std::cell::RefCell<Vec<std::sync::Arc<std::sync::atomic::AtomicBool>>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Ties this thread's browser call to flags that say nobody is waiting for it any more (the tool host gave up on it
/// because of its time limit, or the run was cancelled). A confirmation wait stops at once and the action is not
/// carried out: without this, a click or send could still happen minutes after the caller was told "timed out".
pub struct AbandonBinding;

pub fn bind_abandon(flags: Vec<std::sync::Arc<std::sync::atomic::AtomicBool>>) -> AbandonBinding {
    ABANDON.with(|a| *a.borrow_mut() = flags);
    AbandonBinding
}

impl Drop for AbandonBinding {
    fn drop(&mut self) {
        ABANDON.with(|a| a.borrow_mut().clear());
    }
}

pub fn is_abandoned() -> bool {
    ABANDON.with(|a| a.borrow().iter().any(|f| f.load(Ordering::SeqCst)))
}

thread_local! {
    static PRE_APPROVED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// While this guard is alive, an *action* approval (a Send/Post click, an upload) requested on this thread is
/// granted, because a person already approved exactly this step in a flow (an Approval step before it, the
/// step's own approval prompt, or the flow's own "don't ask" setting). It never grants a site approval
/// (unknown sites stay under the site policy), never covers a desktop tool, and ends with the guard.
pub struct PreApprovedGuard {
    previous: bool,
}

pub fn pre_approved() -> PreApprovedGuard {
    PreApprovedGuard { previous: PRE_APPROVED.with(|p| p.replace(true)) }
}

impl Drop for PreApprovedGuard {
    fn drop(&mut self) {
        PRE_APPROVED.with(|p| p.set(self.previous));
    }
}

pub fn is_pre_approved() -> bool {
    PRE_APPROVED.with(|p| p.get())
}

pub struct ConfirmationBroker {
    pending: Mutex<HashMap<String, (ConfirmRequest, SyncSender<Decision>)>>,
    /// The last questions that ended, and how.
    ended: Mutex<VecDeque<(String, Ended)>>,
    /// What the person typed with a "no" (card OA10-03, AG-11), by question, until the question's asker takes it ([`ConfirmationBroker::take_reason`]).
    reasons: Mutex<VecDeque<(String, String)>>,
    /// The questions the person answered with the run option (card OA6-05, ED-10), until the question's asker takes the mark ([`ConfirmationBroker::take_run_scope`]).
    run_scope: Mutex<VecDeque<String>>,
    counter: AtomicU64,
    events: broadcast::Sender<ConfirmRequest>,
}

impl Default for ConfirmationBroker {
    fn default() -> Self {
        let (events, _) = broadcast::channel(32);
        Self {
            pending: Mutex::new(HashMap::new()),
            ended: Mutex::new(VecDeque::new()),
            reasons: Mutex::new(VecDeque::new()),
            run_scope: Mutex::new(VecDeque::new()),
            counter: AtomicU64::new(1),
            events,
        }
    }
}

/// How many reasons wait to be taken: a question's asker takes its reason the moment it has the answer, so this is a bound on a leak, not a queue.
const MAX_REASONS: usize = 16;

impl ConfirmationBroker {
    /// Subscribe to new requests (the agent stream forwards them to the UI).
    pub fn subscribe(&self) -> broadcast::Receiver<ConfirmRequest> {
        self.events.subscribe()
    }

    pub fn pending(&self) -> Vec<ConfirmRequest> {
        let mut v: Vec<_> = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|(r, _)| r.clone())
            .collect();
        v.sort_by_key(|r| r.created_unix_ms);
        v
    }

    /// Block until the user answers or `timeout` passes (then [`Decision::Deny`]).
    pub fn request(
        &self,
        kind: ConfirmKind,
        tool: &str,
        reason: String,
        url: Option<String>,
        site: Option<String>,
        timeout: Duration,
    ) -> (ConfirmRequest, Decision) {
        self.request_detailed(kind, tool, reason, url, site, false, Vec::new(), timeout)
    }

    /// Like [`request`](Self::request), with what a tool approval needs to show.
    #[allow(clippy::too_many_arguments)]
    pub fn request_detailed(
        &self,
        kind: ConfirmKind,
        tool: &str,
        reason: String,
        url: Option<String>,
        site: Option<String>,
        always_ask: bool,
        classes: Vec<String>,
        timeout: Duration,
    ) -> (ConfirmRequest, Decision) {
        self.request_explained(kind, tool, reason, url, site, always_ask, classes, ConfirmWhy::default(), timeout)
    }

    /// Like [`request_detailed`](Self::request_detailed), with the risk, the rule and the data involved for the prompt to show.
    #[allow(clippy::too_many_arguments)]
    pub fn request_explained(
        &self,
        kind: ConfirmKind,
        tool: &str,
        reason: String,
        url: Option<String>,
        site: Option<String>,
        always_ask: bool,
        classes: Vec<String>,
        why: ConfirmWhy,
        timeout: Duration,
    ) -> (ConfirmRequest, Decision) {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        QUESTIONS_PUT.with(|q| q.set(q.get().wrapping_add(1)));
        let unattended = is_unattended();
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let req = ConfirmRequest {
            request_id: format!("c_{:x}{:04x}", now_ms & 0xffff_ffff, n & 0xffff),
            kind,
            tool: tool.to_string(),
            reason,
            url,
            site,
            created_unix_ms: now_ms,
            expires_in_secs: timeout.as_secs(),
            always_ask,
            classes,
            risk: why.risk,
            rule: why.rule,
            data: why.data,
            expires_unix_ms: now_ms.saturating_add(timeout.as_millis() as u64),
            run_id: asking_run(),
            preview: why.preview,
            run_grant: why.run_grant,
        };
        if kind == ConfirmKind::Action && is_pre_approved() {
            return (req, Decision::Allow);
        }
        if unattended {
            // No prompt, no waiting, no approval: the answer is always "no".
            return (req, Decision::Deny);
        }
        let (tx, rx) = mpsc::sync_channel(1);
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(req.request_id.clone(), (req.clone(), tx));
        let _ = self.events.send(req.clone());
        note_question(QuestionNote::Opened(&req));
        // Waited for in short slices so a call nobody is waiting for any more stops asking.
        let deadline = std::time::Instant::now() + timeout;
        let mut ended = None;
        let decision = loop {
            if is_abandoned() {
                ended = Some(Ended::Withdrawn);
                break Decision::Deny;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                // An answer that came in the last moment is taken, not lost to the clock.
                if let Ok(d) = rx.try_recv() {
                    break d;
                }
                ended = Some(Ended::Expired(req.expires_unix_ms));
                break Decision::Deny;
            }
            match rx.recv_timeout(left.min(Duration::from_millis(100))) {
                Ok(d) => break d,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    ended = Some(Ended::Withdrawn);
                    break Decision::Deny;
                }
            }
        };
        let removed = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&req.request_id);
        // How it ended is written down only when nobody else did (an answer writes its own).
        if let (Some(how), Some(_)) = (ended, removed) {
            self.note_end(&req.request_id, how);
        }
        let outcome = match (decision, ended) {
            (Decision::Allow, _) => "approved",
            (Decision::AllowAlways, _) => "approved_session",
            (Decision::Deny, Some(Ended::Expired(_))) => "timeout",
            (Decision::Deny, Some(Ended::Withdrawn)) => "cancelled",
            (Decision::Deny, _) => "denied",
        };
        note_question(QuestionNote::Closed { request: &req, outcome });
        (req, decision)
    }

    fn note_end(&self, request_id: &str, how: Ended) {
        let mut ended = self.ended.lock().unwrap_or_else(PoisonError::into_inner);
        if ended.iter().any(|(id, _)| id == request_id) {
            return;
        }
        ended.push_back((request_id.to_string(), how));
        while ended.len() > MAX_ENDED {
            ended.pop_front();
        }
    }

    /// How a question ended, when it did and is still remembered.
    pub fn how_ended(&self, request_id: &str) -> Option<Ended> {
        self.ended.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|(id, _)| id == request_id).map(|(_, how)| *how)
    }

    /// An open question, by its id.
    pub fn open_question(&self, request_id: &str) -> Option<ConfirmRequest> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner).get(request_id).map(|(r, _)| r.clone())
    }

    /// Answer a question, and say what came of it: an answer that comes after the question expired is not applied (the run was told no), and one for a question that was
    /// answered, taken back or never asked says so, so that nobody believes a yes that did nothing.
    pub fn resolve_typed(&self, request_id: &str, decision: Decision) -> Resolved {
        let entry = self.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(request_id);
        match entry {
            Some((req, tx)) => {
                // Still on the list, because the waiter looks at the clock in slices: the time is the question's, not the slice's.
                if unix_ms() >= req.expires_unix_ms {
                    self.note_end(request_id, Ended::Expired(req.expires_unix_ms));
                    return Resolved::Expired { at_unix_ms: req.expires_unix_ms };
                }
                match tx.try_send(decision) {
                    Ok(()) => {
                        self.note_end(request_id, Ended::Answered(decision));
                        Resolved::Applied
                    }
                    Err(_) => Resolved::Withdrawn,
                }
            }
            None => match self.how_ended(request_id) {
                Some(Ended::Answered(d)) => Resolved::AlreadyAnswered(d),
                Some(Ended::Expired(at)) => Resolved::Expired { at_unix_ms: at },
                Some(Ended::Withdrawn) => Resolved::Withdrawn,
                None => Resolved::Unknown,
            },
        }
    }

    /// Answer a pending request. Returns false if the answer was not applied (answered, expired, taken back or never asked: see [`resolve_typed`](Self::resolve_typed)).
    pub fn resolve(&self, request_id: &str, decision: Decision) -> bool {
        self.resolve_typed(request_id, decision) == Resolved::Applied
    }

    /// [`resolve_typed`](Self::resolve_typed) for a "no" that comes with words (card OA10-03, AG-11): `reason` is what the person typed, already one line, masked and cut by the
    /// caller. It is kept until the question's asker takes it ([`take_reason`](Self::take_reason)) and is kept for a `Deny` only: a reason that comes with a yes is dropped, so that
    /// words typed next to an approval can never become an instruction. It is put down **before** the answer is handed over, so the asker, which wakes with the answer, finds it.
    pub fn resolve_with_reason(&self, request_id: &str, decision: Decision, reason: Option<&str>) -> Resolved {
        let said = reason.map(str::trim).filter(|r| !r.is_empty() && decision == Decision::Deny);
        let mut put = false;
        if let Some(said) = said {
            // Only a question that is open takes a reason; one that is not is answered as `resolve_typed` answers it.
            if self.open_question(request_id).is_some() {
                let mut reasons = self.reasons.lock().unwrap_or_else(PoisonError::into_inner);
                reasons.retain(|(id, _)| id != request_id);
                reasons.push_back((request_id.to_string(), said.to_string()));
                while reasons.len() > MAX_REASONS {
                    reasons.pop_front();
                }
                put = true;
            }
        }
        let resolved = self.resolve_typed(request_id, decision);
        if put && resolved != Resolved::Applied {
            // The answer was not taken (it came too late, or the run took the question back): the words go with it. Only the words this call put down: an earlier answer's are its own.
            self.reasons.lock().unwrap_or_else(PoisonError::into_inner).retain(|(id, said_now)| !(id == request_id && Some(said_now.as_str()) == reason.map(str::trim)));
        }
        resolved
    }

    /// A plain yes ([`Decision::Allow`]) that carries the run option (card OA6-05, ED-10): the person says yes to this call and to later edits of files in its folder until the run ends. It is taken
    /// only for a question that offered it (`ConfirmRequest::run_grant`), as a yes and nothing else: for a question that did not offer it the answer is the plain yes it would have been, and the mark
    /// is not put down. The mark is put down **before** the answer is handed over, so the asker, which wakes with the answer, finds it ([`take_run_scope`](Self::take_run_scope)). Only the route that
    /// takes the UI token calls this.
    pub fn resolve_for_the_run(&self, request_id: &str) -> Resolved {
        let offered = self.open_question(request_id).is_some_and(|q| q.run_grant.is_some() && q.kind == ConfirmKind::Tool);
        if offered {
            let mut marks = self.run_scope.lock().unwrap_or_else(PoisonError::into_inner);
            marks.retain(|id| id != request_id);
            marks.push_back(request_id.to_string());
            while marks.len() > MAX_REASONS {
                marks.pop_front();
            }
        }
        let resolved = self.resolve_typed(request_id, Decision::Allow);
        if offered && resolved != Resolved::Applied {
            // Not taken (it came too late, or the run took the question back): the mark goes with it.
            self.run_scope.lock().unwrap_or_else(PoisonError::into_inner).retain(|id| id != request_id);
        }
        resolved
    }

    /// Was the question `request_id` answered with the run option? Taken once.
    pub fn take_run_scope(&self, request_id: &str) -> bool {
        let mut marks = self.run_scope.lock().unwrap_or_else(PoisonError::into_inner);
        match marks.iter().position(|id| id == request_id) {
            Some(at) => {
                marks.remove(at);
                true
            }
            None => false,
        }
    }

    /// What the person typed with their "no" to the question `request_id`, taken once; `None` when they typed nothing or it was taken already.
    pub fn take_reason(&self, request_id: &str) -> Option<String> {
        let mut reasons = self.reasons.lock().unwrap_or_else(PoisonError::into_inner);
        let at = reasons.iter().position(|(id, _)| id == request_id)?;
        reasons.remove(at).map(|(_, said)| said)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Card R6-08: the one-door comparison reads how many questions the old routes put on the run's thread, however each one ended.
    #[test]
    fn every_question_put_on_this_thread_is_counted_however_it_ends() {
        let b = ConfirmationBroker::default();
        let start = questions_put_on_this_thread();
        let (_, d) = b.request(ConfirmKind::Action, "browser_click", "Click Send".into(), None, None, Duration::from_millis(20));
        assert_eq!((d, questions_put_on_this_thread() - start), (Decision::Deny, 1), "a question nobody answered");
        {
            let _flow = pre_approved();
            let (_, d) = b.request(ConfirmKind::Action, "browser_click", "Click Send".into(), None, None, Duration::from_millis(20));
            assert_eq!((d, questions_put_on_this_thread() - start), (Decision::Allow, 2), "one a flow had answered already");
        }
        {
            let _alone = unattended();
            let (_, d) = b.request(ConfirmKind::Tool, "win.type", "type".into(), None, None, Duration::from_millis(20));
            assert_eq!((d, questions_put_on_this_thread() - start), (Decision::Deny, 3), "one nobody could be asked");
        }
        // Another thread's questions are not this thread's.
        let other = std::thread::spawn(|| {
            let b = ConfirmationBroker::default();
            let _ = b.request(ConfirmKind::Action, "browser_click", "x".into(), None, None, Duration::from_millis(20));
            questions_put_on_this_thread()
        })
        .join()
        .unwrap();
        assert_eq!(other, 1);
        assert_eq!(questions_put_on_this_thread() - start, 3);
    }

    #[test]
    fn unanswered_request_is_denied_after_timeout() {
        let b = ConfirmationBroker::default();
        let (req, d) = b.request(ConfirmKind::Action, "browser_click", "Click Send".into(), None, None, Duration::from_millis(50));
        assert_eq!(d, Decision::Deny);
        assert!(b.pending().is_empty());
        assert!(!b.resolve(&req.request_id, Decision::Allow));
    }

    fn wait_for_open(b: &ConfirmationBroker) -> ConfirmRequest {
        for _ in 0..500 {
            if let Some(r) = b.pending().into_iter().next() {
                return r;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no question was asked");
    }

    /// Card R6-12: `/browser/confirmations?run=` shows the questions about one run, so a question has to say which run it is about.
    #[test]
    fn a_question_asked_for_a_run_is_listed_with_that_run_and_one_asked_for_none_is_not() {
        let b = Arc::new(ConfirmationBroker::default());
        let asker = {
            let b = b.clone();
            std::thread::spawn(move || {
                let _run = asking_for_run("run_42");
                b.request(ConfirmKind::Tool, "term.run", "run a command".into(), None, None, Duration::from_secs(30)).1
            })
        };
        let open = wait_for_open(&b);
        assert_eq!(open.run_id.as_deref(), Some("run_42"), "{open:?}");
        assert_eq!(b.resolve_typed(&open.request_id, Decision::Allow), Resolved::Applied);
        assert_eq!(asker.join().unwrap(), Decision::Allow);
        // The guard is gone with the thread's work: a question asked after it, or with an empty run, names none.
        {
            let _outer = asking_for_run("run_a");
            let _none = asking_for_run("");
            assert_eq!(asking_run(), None);
        }
        assert_eq!(asking_run(), None);
        let (alone, _) = b.request(ConfirmKind::Tool, "term.run", "run a command".into(), None, None, Duration::from_millis(30));
        assert_eq!(alone.run_id, None);
        assert!(!serde_json::to_string(&alone).unwrap().contains("run_id"), "a question with no run is written as it was");
    }

    #[test]
    fn an_answer_says_what_became_of_its_question_applied_late_twice_taken_back_or_never_asked() {
        let b = Arc::new(ConfirmationBroker::default());
        // Applied, and a second answer to the same question finds it answered.
        let asker = {
            let b = b.clone();
            std::thread::spawn(move || b.request(ConfirmKind::Tool, "term.run", "run a command".into(), None, None, Duration::from_secs(30)).1)
        };
        let open = wait_for_open(&b);
        assert!(open.expires_unix_ms >= open.created_unix_ms + 29_000, "the question says when an answer is too late: {open:?}");
        assert_eq!(b.resolve_typed(&open.request_id, Decision::Allow), Resolved::Applied);
        assert_eq!(asker.join().unwrap(), Decision::Allow);
        assert_eq!(b.resolve_typed(&open.request_id, Decision::Deny), Resolved::AlreadyAnswered(Decision::Allow));
        // Too late: the question expired, the run was told no, and the answer says so (and not that the question is unknown).
        let (_, d) = b.request(ConfirmKind::Tool, "term.run", "run a command".into(), None, None, Duration::from_millis(60));
        assert_eq!(d, Decision::Deny);
        let id = b.ended.lock().unwrap().back().unwrap().0.clone();
        let how = b.how_ended(&id).unwrap();
        assert!(matches!(how, Ended::Expired(_)), "{how:?}");
        assert!(matches!(b.resolve_typed(&id, Decision::Allow), Resolved::Expired { .. }));
        assert!(!b.resolve(&id, Decision::Allow));
        // Never asked.
        assert_eq!(b.resolve_typed("c_nothing", Decision::Allow), Resolved::Unknown);
    }

    #[test]
    fn an_answer_that_comes_after_the_time_is_not_applied_even_while_the_question_is_still_listed() {
        let b = ConfirmationBroker::default();
        // The waiter looks at the clock in slices of 100 ms, so a question can still be listed after its time: it is put there by hand.
        let (tx, rx) = mpsc::sync_channel(1);
        let late = ConfirmRequest { request_id: "c_late".into(), kind: ConfirmKind::Tool, tool: "term.run".into(), reason: String::new(), url: None, site: None, created_unix_ms: 1, expires_in_secs: 60, always_ask: false, classes: vec![], risk: String::new(), rule: String::new(), data: vec![], expires_unix_ms: unix_ms() - 1 , run_id: None, preview: None, run_grant: None };
        b.pending.lock().unwrap().insert("c_late".into(), (late, tx));
        assert!(matches!(b.resolve_typed("c_late", Decision::Allow), Resolved::Expired { .. }));
        assert!(rx.try_recv().is_err(), "the yes was not sent: the run is not told yes after it was told no");
        assert!(b.pending().is_empty());
    }

    /// Card OA6-05 (ED-10): a yes that carries the run option is a plain yes with a mark that the question's asker can take once; the mark is put only on a question that offered the option.
    #[test]
    fn the_run_option_is_a_mark_on_a_yes_and_only_a_question_that_offered_it_takes_it() {
        let b = Arc::new(ConfirmationBroker::default());
        let ask = |offer: Option<&str>, wait: Duration| {
            let b = b.clone();
            let why = ConfirmWhy { run_grant: offer.map(str::to_string), ..Default::default() };
            std::thread::spawn(move || b.request_explained(ConfirmKind::Tool, "fs.edit", "edit a.rs".into(), None, None, true, vec!["WRITE".into()], why, wait))
        };
        // A question that offers it: the yes is a yes, and the mark is there once.
        let asker = ask(Some("Allow edits in src for this run"), Duration::from_secs(30));
        let open = wait_for_open(&b);
        assert_eq!(open.run_grant.as_deref(), Some("Allow edits in src for this run"), "the question says what the option allows");
        assert_eq!(b.resolve_for_the_run(&open.request_id), Resolved::Applied);
        let (asked, decision) = asker.join().unwrap();
        assert_eq!(decision, Decision::Allow, "it is a plain yes");
        assert!(b.take_run_scope(&asked.request_id));
        assert!(!b.take_run_scope(&asked.request_id), "taken once");
        // A question that did not offer it: the same call is a plain yes and no mark is put down.
        let asker = ask(None, Duration::from_secs(30));
        let open = wait_for_open(&b);
        assert!(open.run_grant.is_none() && !serde_json::to_string(&open).unwrap().contains("run_grant"), "a question that offers nothing is written as it was");
        assert_eq!(b.resolve_for_the_run(&open.request_id), Resolved::Applied);
        assert_eq!(asker.join().unwrap().1, Decision::Allow);
        assert!(!b.take_run_scope(&open.request_id), "the option was not offered: no mark");
        // One that is not open, or that ended: nothing is applied, and no mark stays behind.
        assert_eq!(b.resolve_for_the_run("c_nothing"), Resolved::Unknown);
        let asker = ask(Some("Allow edits in src for this run"), Duration::from_millis(60));
        let (late, decision) = asker.join().unwrap();
        assert_eq!(decision, Decision::Deny);
        assert!(matches!(b.resolve_for_the_run(&late.request_id), Resolved::Expired { .. }));
        assert!(!b.take_run_scope(&late.request_id));
    }

    #[test]
    fn a_question_the_run_took_back_says_so_to_a_late_answer() {
        use std::sync::atomic::AtomicBool;
        let b = Arc::new(ConfirmationBroker::default());
        let flag = Arc::new(AtomicBool::new(false));
        let (b2, f2) = (b.clone(), flag.clone());
        let waiter = std::thread::spawn(move || {
            let _bound = bind_abandon(vec![f2]);
            b2.request(ConfirmKind::Action, "browser_click", "Click Send".into(), None, None, Duration::from_secs(30)).1
        });
        let open = wait_for_open(&b);
        flag.store(true, Ordering::SeqCst);
        assert_eq!(waiter.join().unwrap(), Decision::Deny);
        assert_eq!(b.resolve_typed(&open.request_id, Decision::Allow), Resolved::Withdrawn);
    }

    #[test]
    fn a_confirmation_nobody_is_waiting_for_any_more_stops_asking() {
        use std::sync::atomic::AtomicBool;
        let b = std::sync::Arc::new(ConfirmationBroker::default());
        let flag = std::sync::Arc::new(AtomicBool::new(false));
        let (b2, f2) = (b.clone(), flag.clone());
        let started = std::time::Instant::now();
        let waiter = std::thread::spawn(move || {
            let _bound = bind_abandon(vec![f2]);
            b2.request(ConfirmKind::Action, "browser_click", "Click Send".into(), None, None, Duration::from_secs(30)).1
        });
        while b.pending().is_empty() {
            std::thread::sleep(Duration::from_millis(10));
        }
        flag.store(true, Ordering::SeqCst);
        assert_eq!(waiter.join().unwrap(), Decision::Deny);
        assert!(started.elapsed() < Duration::from_secs(5), "it must not wait out the 30 s");
        assert!(b.pending().is_empty(), "the prompt is withdrawn, so a late Allow has nothing to approve");
        // Only the bound thread is affected, and the binding ends with its guard.
        assert!(!is_abandoned());
    }

    #[test]
    fn an_unattended_thread_is_refused_at_once_without_queueing_or_prompting() {
        let b = ConfirmationBroker::default();
        let mut rx = b.subscribe();
        let started = std::time::Instant::now();
        let (_, d) = {
            let _guard = unattended();
            assert!(is_unattended());
            b.request(ConfirmKind::Tool, "term.run", "run a command".into(), None, None, Duration::from_secs(30))
        };
        assert_eq!(d, Decision::Deny);
        assert!(started.elapsed() < Duration::from_secs(2), "it must not wait for anyone");
        assert!(b.pending().is_empty(), "nothing is queued for the UI");
        assert!(rx.try_recv().is_err(), "nothing is announced to the UI");
        // Only the guarded scope is affected, and it ends with the guard.
        assert!(!is_unattended());
        // Another thread is never affected.
        let _g = unattended();
        assert!(!std::thread::spawn(is_unattended).join().unwrap());
    }

    #[test]
    fn a_step_a_person_approved_may_click_send_unattended_but_nothing_else_is_granted() {
        let b = ConfirmationBroker::default();
        let mut rx = b.subscribe();
        let _unattended = unattended();
        let t = Duration::from_secs(30);
        let ask = |kind| b.request(kind, "browser_click", "Click Send".into(), None, Some("example.org".into()), t).1;
        assert_eq!(ask(ConfirmKind::Action), Decision::Deny, "without it, unattended is refused");
        {
            let _ok = pre_approved();
            assert!(is_pre_approved());
            assert_eq!(ask(ConfirmKind::Action), Decision::Allow, "the approved step's own action");
            assert_eq!(ask(ConfirmKind::Site), Decision::Deny, "an unknown site is still the site policy's call");
            assert_eq!(ask(ConfirmKind::Tool), Decision::Deny, "a desktop tool is never covered");
        }
        assert!(!is_pre_approved(), "it ends with the guard");
        assert_eq!(ask(ConfirmKind::Action), Decision::Deny);
        let _g = pre_approved();
        assert!(!std::thread::spawn(is_pre_approved).join().unwrap(), "another thread is never affected");
        assert!(rx.try_recv().is_err() && b.pending().is_empty(), "nothing was queued or shown");
    }

    #[test]
    fn answered_request_returns_decision_and_notifies_subscribers() {
        let b = Arc::new(ConfirmationBroker::default());
        let mut rx = b.subscribe();
        let b2 = Arc::clone(&b);
        let waiter = std::thread::spawn(move || {
            b2.request(ConfirmKind::Site, "browser_navigate", "Open example.org".into(), None, Some("example.org".into()), Duration::from_secs(5)).1
        });
        let req = loop {
            if let Ok(r) = rx.try_recv() {
                break r;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(b.pending().len(), 1);
        assert!(b.resolve(&req.request_id, Decision::AllowAlways));
        assert_eq!(waiter.join().unwrap(), Decision::AllowAlways);
    }

    /// What the observer of the process was told, as `(tool, "opened" | the outcome)`, for the questions of the tool names below only (other tests of this file ask questions too).
    static NOTES: Mutex<Vec<(String, String, Option<String>)>> = Mutex::new(Vec::new());

    fn notes_of(tool: &str) -> Vec<(String, Option<String>)> {
        NOTES.lock().unwrap_or_else(PoisonError::into_inner).iter().filter(|(t, _, _)| t == tool).map(|(_, what, run)| (what.clone(), run.clone())).collect()
    }

    /// Card OA10-01: the observer is told when a question opens (before the asker waits) and how it ends, in the five words the log of the run uses; a question that was never put
    /// (an unattended run's) is not told.
    #[test]
    fn the_observer_is_told_when_a_question_opens_and_how_it_ended() {
        assert!(set_question_observer(|note| {
            let (request, what) = match note {
                QuestionNote::Opened(r) => (r, "opened".to_string()),
                QuestionNote::Closed { request, outcome } => (request, outcome.to_string()),
            };
            NOTES.lock().unwrap_or_else(PoisonError::into_inner).push((request.tool.clone(), what, request.run_id.clone()));
        }), "the first observer is taken");
        assert!(!set_question_observer(|_| {}), "a second one is refused: the first stands");

        let b = Arc::new(ConfirmationBroker::default());
        let ask = |tool: &'static str, wait: Duration, run: &'static str| {
            let b = b.clone();
            std::thread::spawn(move || {
                let _run = asking_for_run(run);
                b.request(ConfirmKind::Tool, tool, "x".into(), None, None, wait).1
            })
        };
        let answer_when_open = |id_tool: &str, decision: Decision| {
            for _ in 0..500 {
                if let Some(r) = b.pending().into_iter().find(|r| r.tool == id_tool) {
                    assert!(b.resolve(&r.request_id, decision));
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("{id_tool} was never asked");
        };
        // Answered: yes, yes for the session, no.
        for (tool, decision, word) in [("probe.yes", Decision::Allow, "approved"), ("probe.session", Decision::AllowAlways, "approved_session"), ("probe.no", Decision::Deny, "denied")] {
            let asker = ask(tool, Duration::from_secs(10), "run_probe");
            answer_when_open(tool, decision);
            assert_eq!(asker.join().unwrap(), decision);
            assert_eq!(notes_of(tool), vec![("opened".to_string(), Some("run_probe".to_string())), (word.to_string(), Some("run_probe".to_string()))], "{tool}");
        }
        // Nobody answered in time.
        assert_eq!(ask("probe.late", Duration::from_millis(60), "run_probe").join().unwrap(), Decision::Deny);
        assert_eq!(notes_of("probe.late").iter().map(|n| n.0.as_str()).collect::<Vec<_>>(), ["opened", "timeout"]);
        // The run that asked went away.
        let gone = {
            let b = b.clone();
            std::thread::spawn(move || {
                let flag = Arc::new(std::sync::atomic::AtomicBool::new(true));
                let _binding = bind_abandon(vec![flag]);
                b.request(ConfirmKind::Tool, "probe.gone", "x".into(), None, None, Duration::from_secs(10)).1
            })
        };
        assert_eq!(gone.join().unwrap(), Decision::Deny);
        assert_eq!(notes_of("probe.gone").iter().map(|n| n.0.as_str()).collect::<Vec<_>>(), ["opened", "cancelled"]);
        // Nobody was asked: nothing to tell.
        {
            let _alone = unattended();
            let _ = b.request(ConfirmKind::Tool, "probe.unattended", "x".into(), None, None, Duration::from_millis(20));
        }
        assert!(notes_of("probe.unattended").is_empty());
    }

    /// Card OA10-03 (AG-11): a "no" can come with words. The words are put down before the answer is handed over, so the asker finds them when it wakes; they are taken once; and they go
    /// with a "no" only (words typed beside a yes are dropped, never an instruction), and with an answer that was applied (a late answer takes none).
    #[test]
    fn words_typed_with_a_no_reach_the_asker_once_and_only_with_a_no_that_was_applied() {
        let b = Arc::new(ConfirmationBroker::default());
        let ask = |tool: &'static str, wait: Duration| {
            let b = b.clone();
            std::thread::spawn(move || {
                let (request, decision) = b.request(ConfirmKind::Tool, tool, "x".into(), None, None, wait);
                (decision, b.take_reason(&request.request_id), b.take_reason(&request.request_id))
            })
        };
        let open = |tool: &str| {
            for _ in 0..500 {
                if let Some(r) = b.pending().into_iter().find(|r| r.tool == tool) {
                    return r;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("{tool} was never asked");
        };
        // A no with words: the asker has them when it wakes, once.
        let asker = ask("reason.words", Duration::from_secs(10));
        let q = open("reason.words");
        assert_eq!(b.resolve_with_reason(&q.request_id, Decision::Deny, Some("edit config.toml instead")), Resolved::Applied);
        assert_eq!(asker.join().unwrap(), (Decision::Deny, Some("edit config.toml instead".to_string()), None));
        // A yes with words: a yes, and the words are dropped.
        let asker = ask("reason.yes_words", Duration::from_secs(10));
        let q = open("reason.yes_words");
        assert_eq!(b.resolve_with_reason(&q.request_id, Decision::Allow, Some("and also delete everything")), Resolved::Applied);
        assert_eq!(asker.join().unwrap(), (Decision::Allow, None, None));
        // A no with no words, and a no with only blanks: none.
        for (tool, words) in [("reason.none", None), ("reason.blank", Some("  \n "))] {
            let asker = ask(tool, Duration::from_secs(10));
            let q = open(tool);
            assert_eq!(b.resolve_with_reason(&q.request_id, Decision::Deny, words), Resolved::Applied);
            assert_eq!(asker.join().unwrap(), (Decision::Deny, None, None), "{tool}");
        }
        // Too late: the question ended without an answer, the words are not kept for anybody, and the earlier answer's words are not taken away by the late one.
        let asker = ask("reason.late", Duration::from_millis(250));
        let q = open("reason.late");
        assert_eq!(asker.join().unwrap().0, Decision::Deny);
        assert!(matches!(b.resolve_with_reason(&q.request_id, Decision::Deny, Some("too late")), Resolved::Expired { .. } | Resolved::Unknown | Resolved::Withdrawn), "a question that ended takes no answer");
        assert_eq!(b.take_reason(&q.request_id), None);
        // A question nobody asked takes none either.
        assert_eq!(b.resolve_with_reason("c_nothing", Decision::Deny, Some("x")), Resolved::Unknown);
        assert_eq!(b.take_reason("c_nothing"), None);
    }
}

//! The two traits a tool source implements: how it does the work, and how a person is asked.

use crate::envelope::{Artifact, ToolError};
use crate::manifest::{PermClass, ToolManifest};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Boxed future, so providers can be used as trait objects without an extra crate.
pub type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Available,
    /// The binary, server or feature is missing. The tool is hidden and calls fail with `UNAVAILABLE`.
    Unavailable(String),
}

/// What a provider says about one concrete call, before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    /// The classes this call will actually exercise, worked out from the arguments.
    pub classes: Vec<PermClass>,
    /// The thing acted on (a path, a host, a repo, a database profile), shown in the prompt.
    pub target: String,
    /// One line shown to the user when the call needs approval.
    pub summary: String,
    /// The target is on the provider's allow list (a trusted domain, an allowed root).
    pub target_allowed: bool,
}

/// What the host tells a provider about the call it is asked to classify (card R6-08): who asks, and what the run holds.
#[derive(Debug, Clone)]
pub struct ClassifyContext {
    pub caller: crate::policy::Caller,
    /// The agent run the call belongs to (empty outside a run).
    pub run: String,
    /// The run has read text from outside (a page, a window, a file).
    pub tainted: bool,
    /// The run holds something private to the person.
    pub private: bool,
}

impl ClassifyContext {
    /// A run that has read nothing and holds nothing, asked by `caller`.
    pub fn clean(caller: crate::policy::Caller) -> Self {
        Self { caller, run: String::new(), tainted: false, private: false }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProviderOutput {
    pub data: Value,
    pub text: String,
    pub artifacts: Vec<Artifact>,
    pub warnings: Vec<String>,
    /// Secret values the provider used. The host masks them in everything it returns or records.
    pub secrets: Vec<String>,
    /// This result is private to the person (a file outside the project, a database row, what a remote machine printed, the clipboard): a run
    /// that has it can leak it, so what it may send out is held to the stricter rules (see `PolicyEngine::decide_caps`).
    pub private: bool,
    /// `Some(false)`: this result came from a source the person vouched for in a way that cannot be changed behind their back (a file whose
    /// content hash they trusted), so it does not mark the run as having read untrusted content for the capability rule. `None`: as the tool's
    /// manifest says. A folder being trusted is not enough: no provider uses this for file reads. The model still gets the text labelled as data.
    pub taints: Option<bool>,
    /// This result is text from outside although the tool's manifest says its output is not (a skill that came from a repository's pack: the person trusted its hash, which is not the same as having written it). The host labels it as
    /// untrusted data and the run counts as having read untrusted text, as for a tool whose manifest says `untrusted_output`. It can only tighten: a provider cannot use it to lift a label the manifest sets.
    pub untrusted: bool,
    /// (Card OA6-05, ED-10) This result is text of files inside a trusted folder of the project the person works in, and nothing else: a read of a file, an outline, a search or a listing of
    /// such a folder. The run still counts as having read text it cannot vouch for (the ordinary rule is unchanged), but not as having read text **from outside the project**, which is
    /// what suspends an edit grant of the run (`crate::run_grant`). Only a provider that knows where the text came from sets it, and never for a file outside a trusted folder, a file of
    /// secrets or kept tool output (which holds web text); the default, and the value of every tool that is not a file tool, is `false`. Ignored when `untrusted` is set.
    pub local: bool,
    /// Not set by any provider and not read by the host. (Card OA5-02) The host keeps the whole redacted text of a result it cuts by itself, in its `SpillStore`, and records the
    /// file as `ToolResult::data["spilled"]` (`@out/<name>`); a provider never writes that file, so that no provider can name a file of its own choosing there. The field stays so
    /// that code written for the earlier `ManagedOutputStore` (`core/src/modules/managed_output.rs`, superseded and never constructed) still compiles.
    pub managed_output_id: Option<String>,
}

impl ProviderOutput {
    /// This result is private to the person: see [`ProviderOutput::private`].
    pub fn marked_private(mut self) -> Self {
        self.private = true;
        self
    }
}

/// Per-call context handed to a provider.
#[derive(Debug, Clone)]
pub struct ExecCtx {
    pub request_id: String,
    pub session_id: String,
    pub agent_run_id: String,
    /// This call's own flag (card OA4-03): the host sets it when the run is cancelled and when the call's time is up. A provider that starts a program, a walk or a wait
    /// polls it and ends that work soon after it turns true; setting it never touches the run's other calls. After it is set the host waits a moment (`STOP_GRACE_MS`) for
    /// the provider's answer and then stops waiting, so a provider that ignores it only loses the chance to say what it had done.
    pub cancel: Arc<AtomicBool>,
    /// How long the host waits for this call, in milliseconds. A provider that has its own limits (a program's time, a network wait) keeps all of them inside this: what it
    /// still runs when the host gives up is ended by `cancel`, but its own account of what happened (the last of the output) can only be given before.
    pub timeout_ms: u64,
    /// Has the run this call belongs to read text from outside (a page, a file, a tool's output) before now? A provider that hands a value on to
    /// something that will use it later (a flow's inputs) says so with it (card R6-11).
    pub run_holds_outside_text: bool,
    /// What the person was shown when they were asked about this call, as the provider that made it described it (`ChangePreview::guard`, card OA6-04, SE-13): the host gives it
    /// back to the provider that made it, so that the call can check, as it makes its change, that what it works on is still what was shown. `None` when nobody was asked about this
    /// call (it ran without a question, or an earlier answer for the session covered it) or the provider showed nothing.
    pub shown: Option<String>,
    /// Where this call's live output goes (card OA7-04, TM-14): a provider whose work prints as it goes (a program) tells it each line, and the app shows the lines while the call runs. `None`
    /// when nobody is shown them: no source was installed in the host, the call is outside a run, or the caller is the voice. Everything told to it is cleaned the way a result is, and none of
    /// it is ever given to the model (see [`crate::progress`]). A provider that does not stream ignores it.
    pub progress: Option<crate::progress::ProgressSink>,
}

/// What a person is shown of a change before they say yes to it (card OA6-04, ED-05, SE-03, UX-07): the text of the change, and a mark of what it was made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangePreview {
    /// The change as a person reads it: for a file, a unified diff (`--- path`, `+++ path`, `@@ ... @@`, lines starting with `-` and `+`) or the first lines of what a new file holds; for a
    /// request that is sent, its method, address and body. The host masks secrets in it, marks every character that cannot be seen, cuts it at a fixed size and says that it did, and
    /// shows it as plain text. It is never given to the model, and the audit log keeps none of it.
    pub text: String,
    /// What the text was made from, in the provider's own words (for a file: its path and a hash of its content). It goes back to the provider with the call (`ExecCtx::shown`), which
    /// refuses the change if what it would change is no longer what was shown. It never leaves the host.
    pub guard: Option<String>,
}

impl ChangePreview {
    /// A preview that nothing is held to.
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into(), guard: None }
    }
}

pub trait ToolProvider: Send + Sync {
    fn id(&self) -> &str;

    fn manifests(&self) -> Vec<ToolManifest>;

    fn availability(&self, _tool: &str) -> Availability {
        Availability::Available
    }

    /// Work out what the call would do. An `Err` refuses the call (bad target, unsupported option).
    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError>;

    /// [`classify`](Self::classify) with what the host knows about the caller and the run (card R6-08). A provider that has nothing to add does not override it.
    /// The host calls this one, never `classify`, so a provider may judge a call by who asks and by what the run has read, and may only say more than `classify`
    /// does (a provider that returned a looser classification here than there would loosen the host).
    fn classify_for(&self, tool: &str, args: &Value, _ctx: &ClassifyContext) -> Result<Classification, ToolError> {
        self.classify(tool, args)
    }

    /// A reason this call must ask the person every time, whatever the mode and whatever they answered before "for this session" (card OA8-03, SE-08): a call that would put
    /// something into the model's context that the person should agree to each time, such as the real values of a file of secrets. The host only ever tightens with it
    /// (a refusal stays a refusal) and shows the reason in the question. `None`: the ordinary rule decides. The host calls it for the same arguments it classified.
    fn always_ask_reason(&self, _tool: &str, _args: &Value) -> Option<String> {
        None
    }

    /// A name for the answer "approve for this session" to cover more than this exact call (card OA4-05, TM-16): for a call that can run code the answer is kept for the call itself (its
    /// tool, arguments and target), so that approving `cargo test -p a` does not approve `cargo test -p b`. A provider that knows a narrow class of calls that are the same to the person (a
    /// project's own test run in a trusted folder, whatever test it names) returns the name of that class, and the answer then covers the calls of the class, for the same tool, caller and
    /// session and for as long as an answer lasts (the target is not part of the key: the name of the class says what the target would). `None`: the exact call, as before. The host reads it only for a call that asks, and never lets it make a call that always asks
    /// stop asking, or a run that has read text from outside use an answer: those rules do not look at it.
    fn grant_scope(&self, _tool: &str, _args: &Value) -> Option<String> {
        None
    }

    /// What this call is when it is one of a project's own checks (the tests, the build, the linter, the type check) in a folder the provider trusts: its kind (`tests`, `build`, `lint`,
    /// `typecheck`) and the folder it runs in (card OA10-05, WF-14). The host reads it only to see whether a pre-approval the person made (`crate::preapproval`) answers the question this
    /// call would be asked; the kind comes from the command line, never from what the model wrote, and a call that does more than run the project's check (a rule asks about it, a push,
    /// a publish) is `None`. `None`: no pre-approval can cover it, which is what every call has always been.
    fn project_check(&self, _tool: &str, _args: &Value) -> Option<crate::preapproval::ProjectCheck> {
        None
    }

    /// What this call would change or send, for the person to read before they answer the question about it (card OA6-04, ED-05, SE-03): a diff for a file, the body of a request. The
    /// host asks for it only when it is about to put a question (and for a dry run), never for a call that runs without one, and works it out for the arguments it classified. `None`: nothing
    /// to show (the provider has no preview for this tool, or the change cannot be worked out, in which case the call itself says why). A provider reads only what it may read for such a call,
    /// and never puts the content of a file of secrets in it: the host replaces the text of a call that uses a credential with a sentence that says it is hidden, whatever the provider says.
    fn approval_preview(&self, _tool: &str, _args: &Value) -> Option<ChangePreview> {
        None
    }

    /// For a call that only changes or creates the text of files (an edit, a write, a patch): the folder that holds each file it would write, as the file guard resolved it (a real path:
    /// no `..`, no link followed out of its folder) (card OA6-05, ED-10). The host reads it only to see whether a run's edit grant (`crate::run_grant`) covers the call, and to name the
    /// folder when the person is offered one; it never decides anything from it alone. `None`, and it is what every tool has always said: the call is something else (a delete, a move, a
    /// copy, a folder made, a command), or the files cannot be worked out. A provider returns `Some` only for a call that nothing but the text of files changes, and never for a name its own
    /// guard refuses to write.
    fn edit_scope(&self, _tool: &str, _args: &Value) -> Option<crate::run_grant::EditScope> {
        None
    }

    /// Does this call send something out of the computer or into another program's window: a form submitted, Enter pressed, a page opened that nobody listed? It
    /// changes nothing in the decision that is applied. The one-door comparison reads it to ask what a run that holds private data (and has read nothing untrusted)
    /// would be asked about (see `PolicyEngine::decide_door`).
    fn sends_something_out(&self, _tool: &str, _args: &Value) -> bool {
        false
    }

    /// Does this provider's tools take part in the one door (card R6-08)? When a request says the switch is on (`RequestContext::door`), the decision that is applied to a
    /// call of such a provider is the door's: the stricter of the ordinary rule and the capability rule, with a send held to what the run has read and holds
    /// (`PolicyEngine::decide_door`). It is the browser's and the desktop's; every other provider keeps the ordinary rule.
    fn door_applies(&self) -> bool {
        false
    }

    /// Other names a skill's list of allowed tools may use for `tool`: the model's own names for the browser and the desktop (`browser_click`, `Click`). A run that a skill
    /// narrowed may call the tool when its own name or one of these is in every list, so a list written for the model's names still means the same through the host.
    fn restriction_names(&self, _tool: &str) -> Vec<String> {
        Vec::new()
    }

    /// May this provider's manifests use the schema keywords `x-clamp` and `x-aliases` (card OA5-03, TL-03: a number moved into its bounds with a note, other spellings of an argument)?
    /// They only widen the spelling of a call, never what it may do, and a call is classified, asked about and audited as the normalised one. Only a provider whose manifests Aqua
    /// wrote says yes: a tool that comes from a server never does, or the server could widen what the host accepts. Default: no.
    fn schema_extensions(&self) -> bool {
        false
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>>;

    /// Look at the result before it is returned (for example re-check a redirect or a path).
    fn post_check(&self, _tool: &str, _out: &ProviderOutput) -> Result<(), ToolError> {
        Ok(())
    }

    /// The run `run_id` is over (card OA12-04): a provider that kept something for a run, such as a language server it started, lets it go. [`crate::host::ToolHost::end_run`] calls it
    /// once for each provider (not once for each of its tools) and never for an empty run id, after the host has forgotten what it kept for the run, and outside every lock of the
    /// host. It must not wait long (the run's own thread is the one that ends it) and must not fail. Default: nothing.
    fn run_ended(&self, _run_id: &str) {}
}

// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub id: String,
    /// The agent run the call belongs to (empty outside a run), so that a question can be found by the run it is about.
    pub agent_run_id: String,
    pub request_id: String,
    pub session_id: String,
    pub tool: String,
    /// The exact action, not a paraphrase.
    pub summary: String,
    pub target: String,
    pub classes: Vec<PermClass>,
    /// Hash of the tool and its normalised arguments: the answer applies to this call only.
    pub digest: String,
    /// `always_ask` requests can be approved once and never remembered.
    pub always_ask: bool,
    pub expires_in_secs: u64,
    /// What the question shows besides the action: the risk, the rule that made it ask, and what is involved.
    pub why: crate::explain::ApprovalWhy,
    /// What the call would change or send (card OA6-04): a diff for a file, the body of a request. Made by the host from the same arguments the call will run with, masked, cut at
    /// [`crate::explain::PREVIEW_MAX_CHARS`] with a line that says so, and plain text only. `None`: the call has none, or it is for a caller that cannot change anything (voice, a
    /// replayed recipe).
    pub preview: Option<String>,
    /// (Card OA6-05, ED-10) Set only when the host would let an answer of [`ApprovalScope::Run`] make an edit grant for this run: the sentence that says plainly what the choice allows (later edits in
    /// this folder, until the run ends). `None` when the switch is off (`AQUA_EDIT_GRANT`), for every call that is not a plain file edit of a run with someone to ask, and for a run that read text
    /// from outside the project: the person is then offered nothing new, and an approver that answers `Run` anyway gets the answer treated as `Once`.
    pub run_grant: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalScope {
    Once,
    /// The same tool and target for the rest of this session.
    Session,
    /// (Card OA6-05, ED-10) This call, and later edits of files in the same folder (and its sub-folders) until this run ends. The folder is the host's own: the one of the call that was
    /// approved, from the provider's `edit_scope`; an approver cannot name one. The host makes a grant only when the request offered it (`ApprovalRequest::run_grant`), and otherwise
    /// treats the answer as `Once`. Only the app's answer (the UI token) can carry it.
    Run,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    Approved(ApprovalScope),
    Denied,
    /// Nobody answered in time. Treated as a refusal.
    Timeout,
}

pub trait Approver: Send + Sync {
    fn request(&self, req: ApprovalRequest) -> ProviderFuture<'_, ApprovalOutcome>;

    /// What the person typed with their "no" to the question `approval_id` (the `id` of its [`ApprovalRequest`]), taken once: `None` when they typed nothing, when this approver
    /// has no place to type it, or when it was taken already (card OA10-03, AG-11). It is the person's own words: one line, secrets masked and cut by whoever kept it, and the
    /// host shows it to the model as data from the person, never as a rule. It cannot approve anything: a reason goes with `Denied` and with nothing else. (It is a method of its own,
    /// and not a field of [`ApprovalOutcome`], so that the outcome stays a small value that is copied about.)
    fn denial_reason(&self, _approval_id: &str) -> Option<String> {
        None
    }
}

/// Refuses everything. The default until a UI is connected.
pub struct DenyAll;

impl Approver for DenyAll {
    fn request(&self, _req: ApprovalRequest) -> ProviderFuture<'_, ApprovalOutcome> {
        Box::pin(async { ApprovalOutcome::Denied })
    }
}

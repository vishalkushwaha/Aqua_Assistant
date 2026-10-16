//! Why a run, or one call of a tool, stopped, in one plain sentence and a few next steps (cards R7-06, R7-07 and R7-14).
//!
//! * [`triage`] names a failure with the twelve classes of the flows supervisor (`flows::supervisor::classify`, called here, not copied)
//!   and writes the sentence by rules ([`explain`]). Fixed text: no model is asked, and the text of the failure is masked for secrets first.
//! * [`diagnose`] asks the local model about a failure the rules could not name, as data in a closed shape, when `AQUA_DIAGNOSE=on`. What it
//!   says is shown as a suggestion and recorded. It is never run.
//! * [`vote`] is the vote over several samples (`AQUA_VOTE`, used by the diagnosis only) and the checker that confirms an answer against facts.
//!
//! Nothing here changes how a run succeeds or fails. It only describes it: the sentence is written into the run's record when the run ends
//! (`agent_runs::Recording::finish`) and `GET /api/v1/runs/{id}` returns it.

pub mod diagnose;
pub mod explain;
pub mod vote;

use crate::agent_runs::AgentRunStore;
use crate::flows::supervisor::classify::{classify, ErrorClass};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::warn;

/// How much of a failure's text is kept (after its secrets are masked).
const MESSAGE_CHARS: usize = 400;
/// How much of it is quoted in a sentence about a class.
const EXCERPT_CHARS: usize = 120;
const NAME_CHARS: usize = 64;

/// A cause the rules know by name. Anything else is [`Cause::ByClass`]: the class says what it is, and when that is *unknown* too the
/// rules have nothing to say and the diagnosis (if it is on) may be asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// A file call named a path outside the folders Aqua may use.
    OutsideFolders,
    /// A tool was called without an argument it needs.
    MissingArgument,
    /// The program behind a tool is not installed.
    ToolNotInstalled,
    /// The tool asked for does not exist.
    WrongToolName,
    /// The person did not approve the call (or did not answer).
    PersonDeclined,
    /// A rule of the tool host (policy, secrets, network rules, protected branches) said no.
    PolicyRefused,
    /// A site asks the person to sign in.
    NeedsLogin,
    /// A request by voice may only look and open.
    VoiceCannotAct,
    /// The browser the request named is not installed.
    BrowserUnavailable,
    /// The run used its time.
    OutOfTime,
    /// The run used all its steps.
    StepLimit,
    /// The run repeated itself and was stopped.
    GoingRoundInCircles,
    /// The model's reply could not be used.
    ModelReply,
    /// The person (or the app) stopped the run.
    Stopped,
    /// Aqua stopped while the run was going.
    Interrupted,
    /// A question to the person went unanswered.
    QuestionExpired,
    /// A limit of the autonomy setting (time, tool calls, files changed, bytes written) stopped the run.
    CapReached,
    /// No particular cause: the class decides.
    ByClass,
}

impl Cause {
    pub fn key(self) -> &'static str {
        match self {
            Cause::OutsideFolders => "outside_folders",
            Cause::MissingArgument => "missing_argument",
            Cause::ToolNotInstalled => "tool_not_installed",
            Cause::WrongToolName => "wrong_tool_name",
            Cause::PersonDeclined => "person_declined",
            Cause::PolicyRefused => "policy_refused",
            Cause::NeedsLogin => "needs_login",
            Cause::VoiceCannotAct => "voice_cannot_act",
            Cause::BrowserUnavailable => "browser_unavailable",
            Cause::OutOfTime => "out_of_time",
            Cause::StepLimit => "step_limit",
            Cause::GoingRoundInCircles => "going_round_in_circles",
            Cause::ModelReply => "model_reply",
            Cause::Stopped => "stopped",
            Cause::Interrupted => "interrupted",
            Cause::QuestionExpired => "question_expired",
            Cause::CapReached => "cap_reached",
            Cause::ByClass => "by_class",
        }
    }

    /// The class a cause belongs to, among the twelve. The text of such a failure is full of paths and names that the keyword rules of
    /// `classify` could mistake for something else, so a cause the rules know decides its class itself. `None`: ask `classify`.
    fn class(self) -> Option<ErrorClass> {
        Some(match self {
            Cause::OutsideFolders | Cause::PersonDeclined | Cause::PolicyRefused | Cause::VoiceCannotAct | Cause::CapReached => ErrorClass::PermissionError,
            Cause::MissingArgument | Cause::WrongToolName => ErrorClass::InvalidInput,
            // A program is a file the tool needs.
            Cause::ToolNotInstalled | Cause::BrowserUnavailable => ErrorClass::MissingFile,
            Cause::NeedsLogin => ErrorClass::Authentication,
            Cause::OutOfTime => ErrorClass::Timeout,
            Cause::ModelReply => ErrorClass::ApiError,
            Cause::StepLimit | Cause::GoingRoundInCircles | Cause::Stopped | Cause::Interrupted | Cause::QuestionExpired => ErrorClass::Unknown,
            Cause::ByClass => return None,
        })
    }

    /// May doing the same again help? `None`: the class says (`ErrorClass::transient`).
    fn retryable(self) -> Option<bool> {
        match self {
            Cause::OutOfTime | Cause::ModelReply | Cause::Interrupted => Some(true),
            Cause::ByClass => None,
            _ => Some(false),
        }
    }
}

/// What the caller knows about how the run ended, besides the failure itself.
#[derive(Debug, Clone, Copy, Default)]
pub struct Context<'a> {
    /// The run's `stopped_reason` (`user_declined`, `policy_denied`, `needs_login`, `deadline`, ...), or empty for a call that is not a run.
    pub stopped_reason: &'a str,
}

/// Why it stopped, for a person.
#[derive(Debug, Clone, PartialEq)]
pub struct Triage {
    pub class: ErrorClass,
    pub cause: Cause,
    /// The tool, as the host names it (cleaned), or empty.
    pub tool: String,
    /// The tool host's error code (`INVALID_ARGUMENTS`, ...), or empty.
    pub code: String,
    /// What the failure said: secrets masked, one line, cut. Not stored with the triage; the diagnosis is asked about it.
    pub message: String,
    pub stopped_reason: String,
    /// One plain sentence.
    pub why: String,
    /// Up to three, in words.
    pub next_steps: Vec<String>,
    /// May a plain retry help?
    pub retryable: bool,
}

impl Triage {
    /// The rules have nothing to say: no cause they know and no class but *unknown*. This, and only this, is what the model may be asked.
    pub fn needs_diagnosis(&self) -> bool {
        self.cause == Cause::ByClass && self.class == ErrorClass::Unknown
    }

    /// What is stored in the run's record and returned by the route.
    pub fn to_value(&self) -> Value {
        json!({
            "source": "rules",
            "class": self.class.key(),
            "class_label": self.class.label(),
            "cause": self.cause.key(),
            "tool": self.tool,
            "why": self.why,
            "next_steps": self.next_steps,
            "retryable": self.retryable,
        })
    }
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// A text from a failure, fit to keep and to show: secrets masked (first, so a secret that straddles the cut does not survive as a prefix),
/// control characters and line breaks turned into single spaces, cut.
pub fn clean(text: &str, max: usize) -> String {
    let masked = aqua_tool_host::output::Redactor::new().redact(text);
    let flat: String = masked.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    cut(&flat.split_whitespace().collect::<Vec<_>>().join(" "), max)
}

/// A tool name as it may appear in a sentence: the characters of a name only.
fn safe_name(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-')).take(NAME_CHARS).collect()
}

/// `ERROR [CODE] tool: message`, the form a failed call has in the agent loop (`agent_tools::observation`), as (code, tool, message).
/// `None` when the text is not in that form.
pub fn parse_observation(obs: &str) -> Option<(String, String, String)> {
    let rest = obs.trim_start().strip_prefix("ERROR")?;
    if !rest.is_empty() && !rest.starts_with(|c: char| c.is_whitespace() || c == '[') {
        return None;
    }
    let rest = rest.trim_start();
    let (code, rest) = match rest.strip_prefix('[').and_then(|r| r.split_once(']')) {
        Some((code, after)) => (code.trim().to_string(), after.trim_start()),
        None => (String::new(), rest),
    };
    Some(match rest.split_once(": ") {
        Some((tool, message)) if !tool.is_empty() && tool.len() <= NAME_CHARS && !tool.contains(char::is_whitespace) => (code, tool.to_string(), message.to_string()),
        _ => (code, String::new(), rest.to_string()),
    })
}

/// A flow step's error text, made of a tool's failure: the code in words, then what the tool said. This is the form `classify` reads.
fn wrap(code: &str, message: &str) -> String {
    let words = code.to_ascii_lowercase().replace('_', " ");
    if words.is_empty() {
        message.to_string()
    } else {
        format!("{words}: {message}")
    }
}

/// The cause the rules can name from the code and the text of a failure (`lower` is the text in ASCII lower case, so that positions agree
/// with the original).
fn detect(code: &str, lower: &str, ctx: &Context) -> Cause {
    // The loop's own word for how the run ended comes first: it is the reason the run stopped.
    match ctx.stopped_reason {
        "user_declined" => return Cause::PersonDeclined,
        "policy_denied" => return Cause::PolicyRefused,
        "needs_login" => return Cause::NeedsLogin,
        "voice_cannot_act" => return Cause::VoiceCannotAct,
        "browser_unavailable" => return Cause::BrowserUnavailable,
        _ => {}
    }
    if matches!(code, "CONFIRMATION_DENIED" | "APPROVAL_DENIED" | "APPROVAL_TIMEOUT" | "APPROVAL_REQUIRED") || lower.contains("the user did not approve") {
        return Cause::PersonDeclined;
    }
    // Before the policy: the host turns this refusal into INVALID_ARGUMENTS on purpose, because a wrong folder can be asked about.
    if lower.contains("outside the folders aqua may use") {
        return Cause::OutsideFolders;
    }
    if lower.contains("missing required argument") || (code == "INVALID_ARGUMENTS" && (lower.contains("is required") || lower.contains("required argument"))) {
        return Cause::MissingArgument;
    }
    if lower.contains("unknown tool") {
        return Cause::WrongToolName;
    }
    if lower.contains("is not installed") {
        return Cause::ToolNotInstalled;
    }
    if matches!(code, "PERMISSION_DENIED" | "SECRET_BLOCKED" | "SANDBOX_VIOLATION" | "NETWORK_BLOCKED" | "PROTECTED_REF" | "PROD_PROTECTED" | "HOST_KEY_UNKNOWN" | "HOST_KEY_MISMATCH")
        || code.starts_with("POLICY_")
        || lower.contains("refused by policy")
        || lower.contains("blocked by policy")
    {
        return Cause::PolicyRefused;
    }
    Cause::ByClass
}

/// What a cause names in the failure's text: the path outside the folders, the argument that was missing, the tool that does not exist.
fn detail_of(cause: Cause, message: &str, lower: &str) -> String {
    let between = |start_marker: &str, end_marker: &str| -> Option<String> {
        let start = lower.find(start_marker)? + start_marker.len();
        let end = start + lower[start..].find(end_marker)?;
        Some(message[start..end].to_string())
    };
    let found = match cause {
        Cause::OutsideFolders => lower.find("' is outside the folders").and_then(|end| message[..end].find('\'').map(|q| message[q + 1..end].to_string())),
        Cause::MissingArgument => between("argument '", "'"),
        Cause::WrongToolName => between("unknown tool '", "'"),
        _ => None,
    };
    cut(found.unwrap_or_default().trim(), 100)
}

/// Why a call of a tool failed. `error_code` is the host's code (`INVALID_ARGUMENTS`, `NOT_FOUND`, ...) or empty; `message` is what the tool
/// said. The text is masked for secrets before anything is made from it.
pub fn triage(tool: &str, error_code: &str, message: &str, ctx: &Context) -> Triage {
    let tool = safe_name(tool);
    let code: String = error_code.trim().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').collect::<String>().to_ascii_uppercase();
    let message = clean(message, MESSAGE_CHARS);
    let lower = message.to_ascii_lowercase();
    let cause = detect(&code, &lower, ctx);
    let class = cause.class().unwrap_or_else(|| classify(&wrap(&code, &message)));
    let words = match cause {
        Cause::ByClass => explain::for_class(class, &tool, &cut(&message, EXCERPT_CHARS)),
        known => explain::for_cause(known, &tool, &detail_of(known, &message, &lower)),
    };
    Triage {
        class,
        cause,
        tool,
        code,
        message,
        stopped_reason: safe_name(ctx.stopped_reason),
        why: words.why,
        next_steps: words.next_steps,
        retryable: cause.retryable().unwrap_or_else(|| class.transient()),
    }
}

/// The triage of a cause that comes from how a run ended and not from a call.
fn from_cause(cause: Cause, ctx: &Context) -> Triage {
    from_cause_naming(cause, "", ctx)
}

/// [`from_cause`], for a cause that names something (the limit that stopped the run).
fn from_cause_naming(cause: Cause, detail: &str, ctx: &Context) -> Triage {
    let words = explain::for_cause(cause, "", &safe_name(detail));
    Triage {
        class: cause.class().unwrap_or(ErrorClass::Unknown),
        cause,
        tool: String::new(),
        code: String::new(),
        message: String::new(),
        stopped_reason: safe_name(ctx.stopped_reason),
        why: words.why,
        next_steps: words.next_steps,
        retryable: cause.retryable().unwrap_or(false),
    }
}

/// A tool call as a run's record kept it, when it failed.
struct FailedCall {
    tool: String,
    code: String,
    message: String,
}

/// The last tool call of a run, if it failed. (An earlier failure the run got past is not why it stopped.)
fn last_failed_call(run: &Value) -> Option<FailedCall> {
    let last = run["steps"].as_array()?.iter().rev().find(|s| s["kind"] == "tool")?;
    if !(last["ok"] == json!(false) || last["status"] == "failed") {
        return None;
    }
    let observation = last["observation"].as_str().unwrap_or("");
    let recorded = last["tool"].as_str().unwrap_or("");
    Some(match parse_observation(observation) {
        Some((code, tool, message)) => FailedCall { tool: if recorded.is_empty() { tool } else { recorded.to_string() }, code, message },
        None => FailedCall { tool: recorded.to_string(), code: String::new(), message: observation.to_string() },
    })
}

/// Why a run that did not finish well stopped, from its record (`AgentRunStore::get`). `None` for a run that completed, is still going,
/// or is waiting for the person's answer.
pub fn for_run(run: &Value) -> Option<Triage> {
    let status = run["status"].as_str()?;
    if !matches!(status, "failed" | "cancelled" | "interrupted" | "expired") {
        return None;
    }
    let reason = run["stopped_reason"].as_str().unwrap_or("");
    let ctx = Context { stopped_reason: reason };
    // A limit of the autonomy setting stopped it (`stopped_by`): that is why, whatever else the loop said.
    if let Some(cap) = run["stopped_by"].as_str().filter(|c| !c.is_empty()) {
        return Some(from_cause_naming(Cause::CapReached, cap, &ctx));
    }
    let last = last_failed_call(run);
    let from_last = || last.as_ref().map(|c| triage(&c.tool, &c.code, &c.message, &ctx));
    Some(match (status, reason) {
        // The loop said how it ended; the last call (if it failed) says which tool and what it was told.
        (_, "user_declined" | "policy_denied" | "needs_login" | "voice_cannot_act" | "browser_unavailable") => from_last().unwrap_or_else(|| triage("", "", "", &ctx)),
        ("cancelled", _) => from_cause(Cause::Stopped, &ctx),
        ("interrupted", _) => from_cause(Cause::Interrupted, &ctx),
        ("expired", _) => from_cause(Cause::QuestionExpired, &ctx),
        (_, "aborted") => from_cause(Cause::Interrupted, &ctx),
        (_, "deadline" | "max_steps" | "stuck") => {
            // What the last call ran into is the better answer when the rules know it: it is what the run kept meeting.
            let own = match reason {
                "deadline" => Cause::OutOfTime,
                "max_steps" => Cause::StepLimit,
                _ => Cause::GoingRoundInCircles,
            };
            from_last().filter(|t| !t.needs_diagnosis()).unwrap_or_else(|| from_cause(own, &ctx))
        }
        (_, r) if r.starts_with("llm_error") || r.starts_with("parse_error") || r == "unparseable_reply" => from_cause(Cause::ModelReply, &ctx),
        (_, r) if r.starts_with("error:") => triage("", "", r.trim_start_matches("error:"), &ctx),
        _ => match from_last() {
            Some(t) => t,
            // The run gave a reason the rules do not know; its answer to the person may say more.
            None => triage("", "", &format!("{} {}", reason.replace('_', " "), run["final_answer"].as_str().unwrap_or("")), &ctx),
        },
    })
}

/// Puts `why_it_stopped`, `next_steps` and `triage` into a run's record as the route returns it. What was stored when the run ended is kept
/// as it was; a run from before this existed, or one that Aqua's start-up or the watchdog ended, is described now, by the same rules.
pub fn add_to_run(run: &mut Value) {
    let stored = run.get("triage").filter(|t| t.is_object()).cloned();
    let triage = stored.or_else(|| for_run(run).map(|t| t.to_value()));
    run["why_it_stopped"] = triage.as_ref().and_then(|t| t.get("why")).cloned().unwrap_or(Value::Null);
    run["next_steps"] = triage.as_ref().and_then(|t| t.get("next_steps")).cloned().unwrap_or_else(|| json!([]));
    run["triage"] = triage.unwrap_or(Value::Null);
    if run.get("diagnosis").is_none() {
        run["diagnosis"] = Value::Null;
    }
}

/// A run has just ended (`agent_runs::Recording::finish`) with `status`: write why it stopped into its record, and, if the rules had nothing to
/// say and `AQUA_DIAGNOSE=on`, ask the model about it in the background. Never stops a run and never fails it.
pub fn on_run_end(store: &Arc<AgentRunStore>, run_id: &str, status: &str) {
    on_run_end_with(store, run_id, status, diagnose::enabled());
}

/// [`on_run_end`] with the switch of the diagnosis given, so that a test does not read the environment.
pub fn on_run_end_with(store: &Arc<AgentRunStore>, run_id: &str, status: &str, diagnosis_on: bool) {
    if !matches!(status, "failed" | "cancelled") {
        return;
    }
    let run = match store.get(run_id) {
        Ok(Some(run)) => run,
        _ => return,
    };
    let Some(t) = for_run(&run) else { return };
    if let Err(e) = store.set_triage(run_id, &t.to_value().to_string()) {
        warn!("{e}");
    }
    if t.needs_diagnosis() && diagnosis_on {
        diagnose::spawn_for_run(store.clone(), run_id.to_string(), diagnose::Input::from_triage(&t));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_runs::{AgentRunStore, NewRun, NewStep, RunEnd, StepOutcome};

    fn of(observation: &str) -> Triage {
        let (code, tool, message) = parse_observation(observation).expect("a failed call in the loop's form");
        triage(&tool, &code, &message, &Context::default())
    }

    #[test]
    fn a_path_outside_the_allowed_folders_is_a_permission_problem_that_can_be_asked_about() {
        let t = of("ERROR [INVALID_ARGUMENTS] fs.write: 'E:/vaibhav_testing/hello.py' is outside the folders Aqua may use. Use a path inside one of: @ws = C:/Users/x/AquaWorkspace If the person named this folder in their request, call fs.allow_folder with it (they are asked); otherwise use a path inside the allowed folders.");
        assert_eq!((t.class, t.cause, t.tool.as_str(), t.retryable), (ErrorClass::PermissionError, Cause::OutsideFolders, "fs.write", false), "{t:?}");
        assert!(t.why.contains("E:/vaibhav_testing/hello.py") && t.why.contains("outside the folders"), "{}", t.why);
        assert_eq!(t.next_steps[0], "Ask me to allow that folder");
        assert!(!t.needs_diagnosis());
    }

    #[test]
    fn a_missing_argument_is_an_invalid_input_and_names_the_argument() {
        let t = of("ERROR [INVALID_ARGUMENTS] fs.read_file: missing required argument 'path'");
        assert_eq!((t.class, t.cause), (ErrorClass::InvalidInput, Cause::MissingArgument), "{t:?}");
        assert!(t.why.contains("'path'") && t.why.contains("without"), "{}", t.why);
        assert!(!t.next_steps.is_empty() && !t.retryable);
    }

    #[test]
    fn a_tool_that_is_not_installed_says_so_and_offers_installing_it() {
        let t = of("ERROR [UNAVAILABLE] docker.ps: Docker is not installed (no docker.exe found). Install Docker Desktop, or set docker_path in data/tools/docker.json.");
        assert_eq!((t.class, t.cause, t.retryable), (ErrorClass::MissingFile, Cause::ToolNotInstalled, false), "{t:?}");
        assert!(t.why.contains("not installed") && t.why.contains("docker.ps"), "{}", t.why);
        assert!(t.next_steps.iter().any(|s| s.contains("Install")), "{:?}", t.next_steps);
    }

    #[test]
    fn a_wrong_tool_name_says_there_is_no_such_tool_and_asks_to_check_the_name() {
        let t = of("ERROR [NOT_FOUND] fs.nope: unknown tool 'fs.nope'");
        assert_eq!((t.class, t.cause), (ErrorClass::InvalidInput, Cause::WrongToolName), "{t:?}");
        assert!(t.why.contains("no tool with that name") && t.why.contains("fs.nope"), "{}", t.why);
        assert!(t.next_steps.iter().any(|s| s.contains("Check the name")), "{:?}", t.next_steps);
    }

    #[test]
    fn a_network_problem_is_the_network_class_and_may_be_tried_again() {
        let t = of("ERROR [BACKEND_ERROR] web.read: error sending request for url (https://example.test/)");
        assert_eq!((t.class, t.cause, t.retryable), (ErrorClass::Network, Cause::ByClass, true), "{t:?}");
        assert!(t.why.contains("connection"), "{}", t.why);
        assert!(t.next_steps.iter().any(|s| s.contains("internet")), "{:?}", t.next_steps);
    }

    #[test]
    fn a_call_the_person_did_not_approve_is_theirs_to_decide_and_is_not_retried() {
        let t = of("ERROR [CONFIRMATION_DENIED] term.run: The user did not approve: Run cargo build. Do not retry this action; tell the user.");
        assert_eq!((t.class, t.cause, t.retryable), (ErrorClass::PermissionError, Cause::PersonDeclined, false), "{t:?}");
        assert!(t.why.contains("You did not approve") && t.why.contains("term.run"), "{}", t.why);
        let folder = of("ERROR [CONFIRMATION_DENIED] fs.allow_folder: The user did not approve: Allow E:/x. Do not retry this action; tell the user.");
        assert!(folder.why.contains("did not allow that folder"), "{}", folder.why);
        assert!(folder.next_steps.iter().any(|s| s.contains("change your mind")), "{:?}", folder.next_steps);
    }

    #[test]
    fn a_refusal_by_a_rule_of_the_host_says_a_rule_and_not_that_the_person_said_no() {
        let t = of("ERROR [PERMISSION_DENIED] fs.read_file: (SECRET_BLOCKED) holds secrets");
        assert_eq!((t.class, t.cause, t.retryable), (ErrorClass::PermissionError, Cause::PolicyRefused, false), "{t:?}");
        assert!(t.why.contains("rule") && !t.why.contains("You did not approve"), "{}", t.why);
        let policy = of("ERROR [POLICY_TOOL_BLOCKED] browser_click: a voice request can read pages");
        assert_eq!(policy.cause, Cause::PolicyRefused);
    }

    #[test]
    fn what_the_rules_cannot_name_stays_unknown_and_is_the_only_thing_the_model_may_be_asked() {
        let odd = triage("web.read", "BACKEND_ERROR", "the thing hiccuped", &Context::default());
        assert_eq!((odd.class, odd.cause, odd.needs_diagnosis()), (ErrorClass::Unknown, Cause::ByClass, true), "{odd:?}");
        assert!(odd.why.contains("does not say why") && odd.why.ends_with("It said: \"the thing hiccuped\""), "{}", odd.why);
        for known in [of("ERROR [TIMEOUT] term.run: no result within 30000 ms"), of("ERROR [NOT_FOUND] fs.read: no such file")] {
            assert!(!known.needs_diagnosis(), "{known:?}");
        }
    }

    #[test]
    fn the_class_comes_from_the_supervisors_rules_when_no_cause_decides_it() {
        // Not copied: whatever `classify` says about the same words, with the code in words in front, is the class.
        for (code, message, class) in [
            ("TIMEOUT", "no result within 30000 ms", ErrorClass::Timeout),
            ("RATE_LIMITED", "too many requests", ErrorClass::RateLimit),
            ("NOT_FOUND", "no such file", ErrorClass::MissingFile),
            ("AUTH_REQUIRED", "session expired, sign in again", ErrorClass::Authentication),
            ("BACKEND_ERROR", "service unavailable", ErrorClass::ExternalService),
        ] {
            let t = triage("web.read", code, message, &Context::default());
            assert_eq!(t.class, classify(&wrap(code, message)), "{code}");
            assert_eq!(t.class, class, "{code}: {t:?}");
            assert_eq!(t.cause, Cause::ByClass);
        }
    }

    #[test]
    fn a_secret_in_the_failure_text_is_masked_in_the_sentence_and_in_everything_that_is_kept() {
        let t = triage("web.read", "BACKEND_ERROR", "could not connect to https://x.test/?api_key=abcdef0123456789 password=hunter2secret99", &Context::default()); // secret-scan: ignore
        let everything = format!("{} {:?} {} {}", t.why, t.next_steps, t.message, t.to_value());
        assert!(!everything.contains("abcdef0123456789") && !everything.contains("hunter2secret99"), "{everything}");
        assert!(t.why.contains("[REDACTED]"), "the quote is there, with the secret masked: {}", t.why);
        assert_eq!(t.class, ErrorClass::Network);
    }

    #[test]
    fn a_long_failure_text_is_cut_and_a_tool_name_with_odd_characters_is_cleaned() {
        let t = triage("fs.read\n<script>alert(1)</script>", "", &"x".repeat(5_000), &Context::default());
        assert!(t.message.chars().count() <= MESSAGE_CHARS + 1, "{}", t.message.chars().count());
        assert_eq!(t.tool, "fs.readscriptalert1script");
        assert!(!t.why.contains('<'), "{}", t.why);
    }

    #[test]
    fn a_failed_call_in_the_loops_form_is_taken_apart_and_other_text_is_not() {
        assert_eq!(parse_observation("ERROR [NOT_FOUND] fs.read: no such file"), Some(("NOT_FOUND".into(), "fs.read".into(), "no such file".into())));
        assert_eq!(parse_observation("  ERROR [BACKEND_ERROR] the page did not load"), Some(("BACKEND_ERROR".into(), String::new(), "the page did not load".into())));
        assert_eq!(parse_observation("ERROR something without a code"), Some((String::new(), String::new(), "something without a code".into())));
        assert_eq!(parse_observation("ERROR [INVALID_ARGUMENTS] fs.write: 'E:/x/y.txt' is outside"), Some(("INVALID_ARGUMENTS".into(), "fs.write".into(), "'E:/x/y.txt' is outside".into())), "a drive letter is not the end of the tool name");
        assert_eq!(parse_observation("all fine"), None);
        assert_eq!(parse_observation("ERRORS found in the file"), None);
        assert_eq!(parse_observation("fs.read a.txt: the file says ERROR [BACKEND_ERROR] but it is only text"), None);
    }

    fn run_json(status: &str, reason: &str, steps: Vec<Value>) -> Value {
        json!({"id": "r", "status": status, "stopped_reason": reason, "final_answer": "", "steps": steps})
    }

    fn tool_step(tool: &str, ok: bool, observation: &str) -> Value {
        json!({"kind": "tool", "tool": tool, "ok": ok, "status": if ok { "completed" } else { "failed" }, "observation": observation})
    }

    #[test]
    fn a_run_that_ended_on_a_failed_call_is_described_by_that_call_when_the_rules_know_it() {
        let outside = "ERROR [INVALID_ARGUMENTS] fs.write: 'E:/x/hello.py' is outside the folders Aqua may use. Use a path inside one of: @ws = C:/w";
        for reason in ["max_steps", "stuck", "deadline"] {
            let t = for_run(&run_json("failed", reason, vec![tool_step("fs.write", false, outside)])).unwrap();
            assert_eq!(t.cause, Cause::OutsideFolders, "{reason}: {t:?}");
        }
        // A failure the run got past is not why it stopped.
        let t = for_run(&run_json("failed", "max_steps", vec![tool_step("fs.write", false, outside), tool_step("fs.list", true, "a.txt")])).unwrap();
        assert_eq!(t.cause, Cause::StepLimit, "{t:?}");
        // The last call failed, and the rules do not know why: the run's own reason is the sentence.
        let t = for_run(&run_json("failed", "deadline", vec![tool_step("web.read", false, "ERROR [BACKEND_ERROR] web.read: the thing hiccuped")])).unwrap();
        assert_eq!((t.cause, t.class), (Cause::OutOfTime, ErrorClass::Timeout), "{t:?}");
        assert!(t.retryable);
    }

    #[test]
    fn each_way_a_run_can_end_has_a_sentence() {
        let t = |status: &str, reason: &str| for_run(&run_json(status, reason, vec![]));
        assert_eq!(t("failed", "stuck").unwrap().cause, Cause::GoingRoundInCircles);
        assert_eq!(t("failed", "max_steps").unwrap().cause, Cause::StepLimit);
        assert_eq!(t("failed", "deadline").unwrap().cause, Cause::OutOfTime);
        assert_eq!(t("failed", "llm_error: the language model is not ready").unwrap().cause, Cause::ModelReply);
        assert_eq!(t("failed", "parse_error_on_last_step: no JSON").unwrap().cause, Cause::ModelReply);
        assert_eq!(t("failed", "aborted").unwrap().cause, Cause::Interrupted);
        assert_eq!(t("interrupted", "process_ended").unwrap().cause, Cause::Interrupted);
        assert_eq!(t("expired", "question_expired").unwrap().cause, Cause::QuestionExpired);
        assert_eq!(t("cancelled", "cancelled").unwrap().cause, Cause::Stopped);
        assert_eq!(t("cancelled", "client_disconnected").unwrap().cause, Cause::Stopped);
        assert_eq!(t("cancelled", "user_declined").unwrap().cause, Cause::PersonDeclined);
        assert_eq!(t("cancelled", "needs_login").unwrap().cause, Cause::NeedsLogin);
        assert_eq!(t("failed", "policy_denied").unwrap().cause, Cause::PolicyRefused);
        assert_eq!(t("failed", "voice_cannot_act").unwrap().cause, Cause::VoiceCannotAct);
        assert_eq!(t("failed", "browser_unavailable").unwrap().cause, Cause::BrowserUnavailable);
        let error = t("failed", "error: connection refused by the host").unwrap();
        assert_eq!((error.cause, error.class), (Cause::ByClass, ErrorClass::Network), "{error:?}");
        // A reason nobody wrote a rule for, and nothing else to go on, is unknown (and may be asked about).
        let other = t("failed", "something_new").unwrap();
        assert!(other.needs_diagnosis() && other.why.contains("does not say why"), "{other:?}");
        // A run that went well, one that is going and one that waits for the person are not described.
        for (status, reason) in [("completed", "final"), ("running", ""), ("stalled", ""), ("waiting_user", "waiting_user")] {
            assert!(t(status, reason).is_none(), "{status}");
        }
    }

    #[test]
    fn a_run_that_a_limit_of_the_autonomy_setting_stopped_says_which_limit() {
        let mut capped = run_json("failed", "final", vec![tool_step("fs.write", true, "Wrote a.txt")]);
        for (cap, words) in [("max_seconds", "how long a run may take"), ("max_tool_calls", "how many tool calls"), ("max_files_changed", "how many files"), ("max_changed_bytes", "how much a run may write"), ("max_something_new", "what a run may do")] {
            capped["stopped_by"] = json!(cap);
            let t = for_run(&capped).unwrap();
            assert_eq!((t.cause, t.class, t.retryable), (Cause::CapReached, ErrorClass::PermissionError, false), "{cap}: {t:?}");
            assert!(t.why.contains(words) && t.why.contains("was reached"), "{cap}: {}", t.why);
            assert!(!t.needs_diagnosis());
        }
        // The cap is why even when the loop gave another reason and a call failed; an empty `stopped_by` is no cap.
        capped["stopped_by"] = json!("max_tool_calls");
        capped["stopped_reason"] = json!("stuck");
        assert_eq!(for_run(&capped).unwrap().cause, Cause::CapReached);
        capped["stopped_by"] = json!("");
        assert_eq!(for_run(&capped).unwrap().cause, Cause::GoingRoundInCircles);
    }

    #[test]
    fn a_declined_call_is_named_from_the_call_that_was_refused() {
        // The loop writes a refused call with no result text: the tool is known, the words are not.
        let refused = json!({"kind": "tool", "tool": "fs.delete", "ok": false, "status": "failed", "decision": "denied", "approval": "denied", "observation": ""});
        let t = for_run(&run_json("cancelled", "user_declined", vec![refused])).unwrap();
        assert_eq!((t.cause, t.tool.as_str()), (Cause::PersonDeclined, "fs.delete"), "{t:?}");
        assert!(t.why.contains("fs.delete"), "{}", t.why);
    }

    fn temp_store(name: &str) -> AgentRunStore {
        let dir = std::env::temp_dir().join(format!("aqua_triage_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        AgentRunStore::open(dir.join("agent_runs.db")).unwrap()
    }

    fn failed_run(s: &AgentRunStore, id: &str, observation: &str, reason: &str) {
        s.create_run(&NewRun { id: id.into(), started_at: 1_000, goal_redacted: "write hello.py".into(), ..Default::default() }).unwrap();
        let seq = s.add_step(id, &NewStep { step: 1, kind: "tool".into(), tool: "fs.write".into(), ts: 1_001, ..Default::default() }).unwrap();
        s.finish_step(id, seq, &StepOutcome { ok: false, status: "failed".into(), observation_head: observation.into(), decision: "error".into(), ..Default::default() }).unwrap();
        s.end_run(id, 2_000, &RunEnd { status: "failed".into(), stopped_reason: reason.into(), rounds: 3, ..Default::default() }).unwrap();
    }

    #[test]
    fn a_run_record_made_before_this_existed_loads_and_is_described_when_it_failed() {
        let dir = std::env::temp_dir().join(format!("aqua_triage_v3_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            // A database as migration 0003 left it: no column for the sentence, a run that failed and one that went well.
            let c = rusqlite::Connection::open(dir.join("agent_runs.db")).unwrap();
            c.execute_batch(
                "CREATE TABLE agent_runs (id TEXT PRIMARY KEY, started_at INTEGER NOT NULL, ended_at INTEGER, goal_redacted TEXT NOT NULL DEFAULT '', caller TEXT NOT NULL DEFAULT '', route TEXT NOT NULL DEFAULT '', status TEXT NOT NULL, model TEXT NOT NULL DEFAULT '', rounds INTEGER NOT NULL DEFAULT 0, tokens_in INTEGER NOT NULL DEFAULT 0, tokens_out INTEGER NOT NULL DEFAULT 0, stopped_reason TEXT NOT NULL DEFAULT '', final_answer_head TEXT NOT NULL DEFAULT '', parent_run_id TEXT, plan_json TEXT, taint_json TEXT, goal_full TEXT NOT NULL DEFAULT '');
                 CREATE TABLE agent_steps (run_id TEXT NOT NULL, step INTEGER NOT NULL, seq INTEGER NOT NULL, kind TEXT NOT NULL, tool TEXT NOT NULL DEFAULT '', args_redacted TEXT NOT NULL DEFAULT '', observation_digest TEXT NOT NULL DEFAULT '', observation_head TEXT NOT NULL DEFAULT '', ok INTEGER, status TEXT NOT NULL DEFAULT 'started', decision TEXT NOT NULL DEFAULT '', approval TEXT NOT NULL DEFAULT '', ms INTEGER, tokens_in INTEGER, tokens_out INTEGER, cache_read INTEGER, planner_reply TEXT NOT NULL DEFAULT '', idempotency_digest TEXT NOT NULL DEFAULT '', ts INTEGER NOT NULL, ttft_ms INTEGER, decode_tps_x10 INTEGER, PRIMARY KEY (run_id, seq));
                 INSERT INTO agent_runs (id, started_at, status, stopped_reason) VALUES ('old_failed', 1, 'failed', 'deadline');
                 INSERT INTO agent_runs (id, started_at, status, stopped_reason) VALUES ('old_fine', 2, 'completed', 'final');
                 PRAGMA user_version = 3;",
            )
            .unwrap();
        }
        let s = AgentRunStore::open(dir.join("agent_runs.db")).unwrap();
        let failed = s.get("old_failed").unwrap().unwrap();
        assert!(failed["why_it_stopped"].as_str().unwrap().contains("ran out of time"), "{failed}");
        assert_eq!(failed["triage"]["cause"], "out_of_time");
        assert!(!failed["next_steps"].as_array().unwrap().is_empty());
        assert_eq!(failed["diagnosis"], Value::Null);
        let fine = s.get("old_fine").unwrap().unwrap();
        assert_eq!((fine["why_it_stopped"].clone(), fine["next_steps"].clone(), fine["triage"].clone()), (Value::Null, json!([]), Value::Null), "{fine}");
        assert_eq!(fine["status"], "completed", "what was there is still there");
        assert!(s.triage_json("old_failed").unwrap().is_none(), "describing an old run writes nothing");
        drop(s);
        assert!(AgentRunStore::open(dir.join("agent_runs.db")).is_ok(), "opening it again does not try the upgrade twice");
    }

    #[test]
    fn when_a_run_ends_badly_the_sentence_is_written_into_its_record_and_what_was_written_is_kept() {
        let s = Arc::new(temp_store("on_end"));
        failed_run(&s, "r1", "ERROR [INVALID_ARGUMENTS] fs.write: 'E:/x/hello.py' is outside the folders Aqua may use. Use a path inside one of: @ws = C:/w", "max_steps");
        on_run_end_with(&s, "r1", "failed", false);
        let stored: Value = serde_json::from_str(&s.triage_json("r1").unwrap().expect("written when the run ended")).unwrap();
        assert_eq!((stored["cause"].as_str(), stored["class"].as_str(), stored["source"].as_str()), (Some("outside_folders"), Some("permission_error"), Some("rules")));
        let run = s.get("r1").unwrap().unwrap();
        assert_eq!(run["why_it_stopped"], stored["why"]);
        assert_eq!(run["next_steps"], stored["next_steps"]);
        // What was stored is what the person saw then: a later change of the words does not rewrite it.
        s.set_triage("r1", &json!({"why": "As it was said then.", "next_steps": ["Do this"], "cause": "outside_folders"}).to_string()).unwrap();
        let run = s.get("r1").unwrap().unwrap();
        assert_eq!((run["why_it_stopped"].as_str(), run["next_steps"][0].as_str()), (Some("As it was said then."), Some("Do this")));
        // A run that went well, or that waits for an answer, gets nothing.
        s.create_run(&NewRun { id: "fine".into(), started_at: 5, ..Default::default() }).unwrap();
        s.end_run("fine", 6, &RunEnd { status: "completed".into(), stopped_reason: "final".into(), ..Default::default() }).unwrap();
        on_run_end_with(&s, "fine", "completed", false);
        assert!(s.triage_json("fine").unwrap().is_none());
        on_run_end_with(&s, "nope", "failed", false);
    }

    #[test]
    fn the_sentence_for_a_run_is_masked_even_when_the_record_held_a_secret() {
        let s = Arc::new(temp_store("masked"));
        failed_run(&s, "r2", "ERROR [BACKEND_ERROR] web.read: the thing hiccuped token=ghp_abcdefghijklmnopqrstuvwxyz0123456789", "failed_call"); // secret-scan: ignore
        on_run_end_with(&s, "r2", "failed", false);
        let run = s.get("r2").unwrap().unwrap();
        let shown = format!("{} {} {}", run["why_it_stopped"], run["next_steps"], run["triage"]);
        assert!(!shown.contains("ghp_abcdefghijklmnopqrstuvwxyz0123456789"), "{shown}"); // secret-scan: ignore
        assert_eq!(run["triage"]["cause"], "by_class");
    }
}

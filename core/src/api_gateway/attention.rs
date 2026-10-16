//! Say, and show, when a run needs the person or has ended (card OA10-03; task UX-13 of the OpenCode audit).
//!
//! A detached run (`run_host`) goes on with nobody looking at it, and a question of the host about one of its calls is a "no" after `approval_wait_secs`: unseen, it only times out. This puts
//! a short sentence in the queue of spoken announcements (`flows::notify_voice`, the queue the app's player empties at `POST /api/v1/announcements/next`, which says it with the speech
//! engine when it is not busy and which the app can show as a toast at the same time) when
//!
//! * a question of the host about a call is open ("I need your yes to change a file. It is on the screen."; with several: "I need your yes on 3 things. They are on the screen." as one
//!   sentence, not three), and is taken back when it was answered, so that nobody is told something that is not true any more;
//! * a detached run ended ("Done." / "Done: 3 files changed.", "I stopped: I reached the limit of 200 tool calls.", "I stopped: I ran out of time.", "I stopped: nobody answered my question.",
//!   "I have a question for you. It is on the screen." when it ended with the planner's question, "I stopped: something went wrong. It is on the screen."). A run the person stopped says
//!   nothing, and neither does one that was refused: they know. A run that is not a detached one says nothing at its end: its answer, or its question, is spoken as it always was.
//!
//! **Nothing is said unless `AQUA_ATTENTION` is on**, and then only a sentence from the fixed templates below and, for a question, the name of the file when the action names one: never
//! code, a path or what a call touched. Nothing here approves anything: a "yes" is never taken by voice, and the sentence says the question is on the screen.
//!
//! The core cannot know whether the app is in front, so it always queues and the player decides (it asks `busy` first, as it does for reminders). Which events to say, quiet hours and
//! "only when the app is not in focus" are the app's settings (`documentation/NOTIFICATIONS.md`); this is the core's half.

use super::run_host::Ended;
use crate::agent_runs::{self, Attention};
use crate::flows::notify_voice::queue;
use aqua_mcp_adapters::browser::ConfirmRequest;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The kinds of announcement this makes (the `kind` of `notify_voice::Announcement`, which the app can show as a toast too).
pub const KIND_APPROVAL: &str = "approval_waiting";
pub const KIND_QUESTION: &str = "run_question";
pub const KIND_DONE: &str = "run_done";
pub const KIND_STOPPED: &str = "run_stopped";

/// The longest file name that is said.
const MAX_NAME_CHARS: usize = 40;

/// Questions of the host that are open now, for the sentence that counts them.
static OPEN: AtomicUsize = AtomicUsize::new(0);

fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn enabled() -> bool {
    crate::flags::is_on(&crate::flags::ATTENTION)
}

/// Makes the core tell the queue of announcements what needs the person. Called once, when the gateway starts; the first call wins.
pub fn install() {
    agent_runs::set_attention_sink(|what| on_event(what, &queue()));
}

fn on_event(what: &Attention<'_>, queue: &crate::flows::notify_voice::VoiceQueue) {
    // The count is kept whether or not anything is said, so that switching the setting on in the middle of a run does not make it wrong.
    match what {
        Attention::ApprovalOpened { .. } => {
            OPEN.fetch_add(1, Ordering::SeqCst);
        }
        Attention::ApprovalClosed { .. } => {
            let _ = OPEN.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| Some(n.saturating_sub(1)));
        }
    }
    if !enabled() {
        return;
    }
    let now = unix_now();
    match what {
        Attention::ApprovalOpened { request, .. } => {
            queue.push_replacing(approval_sentence(request, OPEN.load(Ordering::SeqCst)), KIND_APPROVAL, now);
        }
        Attention::ApprovalClosed { .. } => {
            if OPEN.load(Ordering::SeqCst) == 0 {
                queue.withdraw(KIND_APPROVAL, now);
            }
        }
    }
}

/// A detached run ended: the sentence for how, if it is one to say.
pub fn run_ended(run_id: &str, ended: &Ended) {
    if !enabled() {
        return;
    }
    let (changed, limit) = facts_of(run_id);
    if let Some((kind, text)) = ended_sentence(ended, changed, limit.as_ref()) {
        queue().push(text, kind, unix_now());
    }
}

/// What the log of the run says that the sentence needs: how many files it changed and the cap that stopped it, if one did.
fn facts_of(run_id: &str) -> (u64, Option<(String, u64)>) {
    let Some(store) = agent_runs::store() else { return (0, None) };
    let rows = store.events_after(run_id, 0, 1_000).unwrap_or_default();
    let changed = rows.iter().rev().find(|r| r.kind == "run.finished").and_then(|r| r.data.get("files_changed").and_then(serde_json::Value::as_u64)).unwrap_or(0);
    let limit = rows.iter().rev().find(|r| r.kind == "limit").and_then(|r| Some((r.data.get("stopped_by")?.as_str()?.to_string(), r.data.get("cap")?.as_u64()?)));
    (changed, limit)
}

/// What a call is, in words that say it without saying what it touches.
fn what_it_does(tool: &str) -> &'static str {
    let t = tool.to_ascii_lowercase();
    if ["fs.write", "fs.edit", "fs.patch", "fs.delete", "fs.move", "fs.copy", "fs.undo", "fs.mkdir", "fs.trash"].iter().any(|p| t.starts_with(p)) {
        "change a file"
    } else if t.starts_with("fs.") {
        "use your files"
    } else if t.starts_with("term.") {
        "run a command"
    } else if t.starts_with("git.") {
        "use git"
    } else if t.starts_with("browser") || t.starts_with("web.") || t.starts_with("http.") {
        "act on the web"
    } else if t.starts_with("ssh.") || t.starts_with("docker.") || t.starts_with("k8s.") || t.starts_with("iac.") || t.starts_with("cloud.") {
        "run something on another machine"
    } else {
        "do something"
    }
}

/// The name of the file a question is about, when the action names one: the last part of the first thing in its summary that looks like a file name, in letters, digits and `._-` only.
fn file_name_in(summary: &str) -> Option<String> {
    summary.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '(' | ')' | ',' | ';')).find_map(|word| {
        let name = word.rsplit(['/', '\\']).next()?;
        let looks_like_a_file = name.len() <= MAX_NAME_CHARS && name.contains('.') && !name.starts_with('.') && !name.ends_with('.') && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        // A name with a long run of digits or letters could be a key or a token: it is not said.
        let plain = name.split(['.', '_', '-']).all(|part| part.len() <= 20);
        (looks_like_a_file && plain).then(|| name.to_string())
    })
}

/// "I need your yes to change a file." / "... to edit main.rs." with the question's own words only for the kind of action and a file name; several at once are one sentence.
pub fn approval_sentence(request: &ConfirmRequest, open: usize) -> String {
    if open > 1 {
        return format!("I need your yes on {open} things. They are on the screen.");
    }
    let does = what_it_does(&request.tool);
    let named = (does == "change a file").then(|| file_name_in(&request.reason)).flatten();
    match named {
        Some(name) => format!("I need your yes to change {name}. It is on the screen."),
        None => format!("I need your yes to {does}. It is on the screen."),
    }
}

pub fn question_sentence() -> String {
    "I have a question for you. It is on the screen.".to_string()
}

/// With AQUA_CODING_VOICE on, a run that changed files also says how to take them back by voice (card OA10-06, UX-13 and UX-16): the phrase puts a card on the screen, and the person presses the button.
fn with_undo_hint(sentence: String) -> String {
    if crate::coding_voice::enabled() {
        format!("{sentence} Say undo the last run if you want them back.")
    } else {
        sentence
    }
}

/// The sentence for how a detached run ended and its kind, or `None` when nothing is to be said (the person ended it, or said no to it themselves).
pub fn ended_sentence(ended: &Ended, files_changed: u64, limit: Option<&(String, u64)>) -> Option<(&'static str, String)> {
    match ended.outcome {
        "done" => Some((
            KIND_DONE,
            match files_changed {
                0 => "Done.".to_string(),
                1 => with_undo_hint("Done: 1 file changed.".to_string()),
                n => with_undo_hint(format!("Done: {n} files changed.")),
            },
        )),
        "needs_you" if ended.stopped_reason == "waiting_user" => Some((KIND_QUESTION, question_sentence())),
        // A plan run wrote its plan (card OA10-04, AG-06): it waits for the person's yes, which only the app can give.
        "needs_you" if ended.stopped_reason == "plan_ready" => Some((KIND_QUESTION, "I have a plan ready. It needs your yes before I build it. It is on the screen.".to_string())),
        "needs_you" => Some((KIND_STOPPED, "I stopped: nobody answered my question.".to_string())),
        "timed_out" => Some((KIND_STOPPED, "I stopped: I ran out of time.".to_string())),
        "cancelled" => None,
        _ if ended.stopped_reason == "user_declined" => None,
        _ => Some((
            KIND_STOPPED,
            match limit {
                Some((cap, n)) if cap == "max_tool_calls" => format!("I stopped: I reached the limit of {n} tool calls."),
                Some((cap, n)) if cap == "max_files_changed" => format!("I stopped: I reached the limit of {n} changed files."),
                Some((cap, _)) if cap == "max_changed_bytes" => "I stopped: I reached the limit of changed data.".to_string(),
                Some((cap, _)) if cap == "max_seconds" => "I stopped: I ran out of time.".to_string(),
                _ => "I stopped: something went wrong. It is on the screen.".to_string(),
            },
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flows::notify_voice::VoiceQueue;

    fn request(tool: &str, reason: &str) -> ConfirmRequest {
        ConfirmRequest {
            request_id: "c_1".into(),
            kind: aqua_mcp_adapters::browser::ConfirmKind::Tool,
            tool: tool.into(),
            reason: reason.into(),
            url: None,
            site: None,
            created_unix_ms: 0,
            expires_in_secs: 60,
            always_ask: false,
            classes: vec![],
            risk: String::new(),
            rule: String::new(),
            data: vec![],
            expires_unix_ms: 0,
            run_id: Some("run_1".into()),
            preview: None,
            run_grant: None,
        }
    }

    fn done(outcome: &'static str, reason: &str) -> Ended {
        Ended { status: "x".into(), stopped_reason: reason.into(), outcome, answer_head: String::new() }
    }

    #[test]
    fn the_sentences_are_fixed_templates_with_at_most_a_file_name_and_never_a_path_or_code() {
        let say = |tool: &str, reason: &str, open: usize| approval_sentence(&request(tool, reason), open);
        assert_eq!(say("fs.write", "Write main.rs in @ws/src", 1), "I need your yes to change main.rs. It is on the screen.");
        assert_eq!(say("fs.edit", "Edit E:\\work\\secret project\\src\\lib.rs", 1), "I need your yes to change lib.rs. It is on the screen.", "only the last part of a path");
        assert_eq!(say("fs.write", "Write all of it", 1), "I need your yes to change a file. It is on the screen.", "no file name in the summary, none is said");
        assert_eq!(say("term.run", "Run cargo build --release in E:\\x", 1), "I need your yes to run a command. It is on the screen.", "what a command is made of is not said");
        assert_eq!(say("browser_click", "Click Send", 1), "I need your yes to act on the web. It is on the screen.");
        assert_eq!(say("mystery.tool", "x", 1), "I need your yes to do something. It is on the screen.");
        assert_eq!(say("fs.write", "Write main.rs", 3), "I need your yes on 3 things. They are on the screen.", "several are one sentence with a count");
        // A name that could be a key is not said.
        let payment_key_filename = ["Write sk_", "live_", "aaaaaaaaaaaaaaaaaaaaaaaa", ".txt"].concat();
        assert_eq!(say("fs.write", &payment_key_filename, 1), "I need your yes to change a file. It is on the screen.");
        assert_eq!(say("fs.write", "Write a file with a very long name aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.txt", 1), "I need your yes to change a file. It is on the screen.");
        assert_eq!(question_sentence(), "I have a question for you. It is on the screen.");
    }

    #[test]
    fn how_a_run_ended_is_said_when_it_is_for_the_person_to_hear_and_not_when_they_did_it_themselves() {
        let say = |ended: Ended, changed: u64, limit: Option<(String, u64)>| ended_sentence(&ended, changed, limit.as_ref()).map(|(k, t)| (k, t));
        assert_eq!(say(done("done", "final"), 0, None), Some((KIND_DONE, "Done.".to_string())));
        assert_eq!(say(done("done", "final"), 1, None), Some((KIND_DONE, "Done: 1 file changed.".to_string())));
        assert_eq!(say(done("done", "final"), 3, None), Some((KIND_DONE, "Done: 3 files changed.".to_string())));
        assert_eq!(say(done("failed", "max_steps"), 0, Some(("max_tool_calls".into(), 200))), Some((KIND_STOPPED, "I stopped: I reached the limit of 200 tool calls.".to_string())));
        assert_eq!(say(done("failed", "final"), 0, Some(("max_seconds".into(), 900))), Some((KIND_STOPPED, "I stopped: I ran out of time.".to_string())));
        assert_eq!(say(done("failed", "error"), 0, None), Some((KIND_STOPPED, "I stopped: something went wrong. It is on the screen.".to_string())));
        assert_eq!(say(done("timed_out", "deadline"), 0, None), Some((KIND_STOPPED, "I stopped: I ran out of time.".to_string())));
        assert_eq!(say(done("needs_you", "user_declined"), 0, None), Some((KIND_STOPPED, "I stopped: nobody answered my question.".to_string())));
        assert_eq!(say(done("needs_you", "waiting_user"), 0, None), Some((KIND_QUESTION, question_sentence())));
        // The person stopped it, or said no to it: they know.
        assert_eq!(say(done("cancelled", "cancelled"), 0, None), None);
        assert_eq!(say(done("failed", "user_declined"), 0, None), None);
    }

    /// Card OA10-06 (UX-13 and UX-16): with the coding phrases on, a run that changed files says how to take the changes back by voice; with them off, and for a run that changed nothing, the sentences are what they were.
    #[test]
    fn a_run_that_changed_files_says_how_to_undo_them_by_voice_only_when_the_coding_phrases_are_on() {
        let say = |changed: u64| ended_sentence(&done("done", "final"), changed, None).map(|(_, text)| text).unwrap();
        crate::flags::with_overrides(&[("AQUA_CODING_VOICE", Some("on"))], || {
            assert_eq!(say(3), "Done: 3 files changed. Say undo the last run if you want them back.");
            assert_eq!(say(1), "Done: 1 file changed. Say undo the last run if you want them back.");
            assert_eq!(say(0), "Done.", "nothing to take back, nothing to say");
        });
        crate::flags::with_overrides(&[("AQUA_CODING_VOICE", None)], || assert_eq!(say(3), "Done: 3 files changed."));
    }

    /// Card OA10-04 (AG-06): a plan run that wrote its plan is a run that needs the person's yes, and the sentence says so without a word of the plan.
    #[test]
    fn a_plan_that_waits_for_the_person_is_said_as_a_plan_and_a_plan_run_is_not_a_run_that_ended_well() {
        let ended = Ended { status: "completed".into(), stopped_reason: "plan_ready".into(), outcome: crate::queue::outcome_of("plan_ready"), answer_head: "1) edit secret.rs".into() };
        assert_eq!(ended.outcome, "needs_you", "what is kept for after it is not started on a guess");
        let (kind, text) = ended_sentence(&ended, 0, None).expect("it is said");
        assert_eq!((kind, text.as_str()), (KIND_QUESTION, "I have a plan ready. It needs your yes before I build it. It is on the screen."));
        assert_eq!(crate::agent_runs::status_for("plan_ready"), "completed");
    }

    #[test]
    fn a_question_is_said_once_with_a_count_and_taken_back_when_it_was_answered() {
        // (The count is the process's: this test is the only one that opens and closes questions through `on_event`.)
        let q = VoiceQueue::default();
        let (a, b) = (request("fs.write", "Write a.rs"), request("term.run", "Run it"));
        let was = OPEN.load(Ordering::SeqCst);
        OPEN.store(0, Ordering::SeqCst);
        crate::flags::with_overrides(&[("AQUA_ATTENTION", Some("on"))], || {
            on_event(&Attention::ApprovalOpened { run_id: "run_1", request: &a }, &q);
            assert_eq!(q.next(unix_now()).map(|x| x.text), Some("I need your yes to change a.rs. It is on the screen.".to_string()));
            q.ack(1, false, unix_now());
            on_event(&Attention::ApprovalOpened { run_id: "run_1", request: &b }, &q);
            assert_eq!(q.waiting(), 1, "the second took the place of the first");
            assert_eq!(q.next(unix_now()).map(|x| x.text), Some("I need your yes on 2 things. They are on the screen.".to_string()));
            q.ack(2, true, unix_now());
            // Both were answered since: nothing is open now. Two more are put, and one is answered: the other is still open, so what is waiting is not taken back; both answered:
            // nothing is left to say.
            OPEN.store(0, Ordering::SeqCst);
            on_event(&Attention::ApprovalOpened { run_id: "run_1", request: &a }, &q);
            on_event(&Attention::ApprovalOpened { run_id: "run_1", request: &b }, &q);
            on_event(&Attention::ApprovalClosed { run_id: "run_1", request: &a }, &q);
            assert_eq!(q.waiting(), 1);
            on_event(&Attention::ApprovalClosed { run_id: "run_1", request: &b }, &q);
            assert_eq!(q.waiting(), 0, "answered before it was said: not said");
        });
        // With the setting off nothing is queued, however many questions open.
        crate::flags::with_overrides(&[("AQUA_ATTENTION", None)], || {
            on_event(&Attention::ApprovalOpened { run_id: "run_1", request: &a }, &q);
            assert_eq!(q.waiting(), 0);
            on_event(&Attention::ApprovalClosed { run_id: "run_1", request: &a }, &q);
        });
        OPEN.store(was, Ordering::SeqCst);
    }

    #[test]
    fn a_name_that_names_a_file_is_found_and_nothing_else_is() {
        assert_eq!(file_name_in("Write notes.txt in @ws"), Some("notes.txt".to_string()));
        assert_eq!(file_name_in("Edit \"E:\\a b\\c.rs\""), Some("c.rs".to_string()));
        assert_eq!(file_name_in("Edit @ws/src/mod.rs"), Some("mod.rs".to_string()));
        for no in ["Write everything", "Run cargo test", ".env", "Write ..", "Delete data."] {
            assert_eq!(file_name_in(no), None, "{no}");
        }
    }
}

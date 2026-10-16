//! `aqua-cli run`: start a run on the core and follow it to its end, with an exit code (card OA10-05; tasks UX-05 and WF-14 of the OpenCode audit).
//!
//! The run is started detached (`POST /api/v1/runs`) and followed on its stream (`GET /api/v1/runs/{id}/stream?after=`), so a connection that breaks does not end the run and the client
//! takes the stream up again from the last event it has: it misses nothing and prints nothing twice. **The CLI never answers a question of the core**: a run that needs the person says so
//! (the app is where the answer is given) and the exit code is 2.
//!
//! The following itself is [`watch`], which does not know what is done with an event: a [`Watcher`] is told each frame and asked what it wants between two reads. `follow` (text or JSON on a
//! terminal) is one watcher; the terminal view (`view.rs`, card OA10-07) and the editor protocol (`acp.rs`, card OA10-07) are two more, and all three take a broken stream up again the same way.
//!
//! | Exit | Means |
//! |---|---|
//! | 0 | the run completed |
//! | 1 | the run failed, or something in the request was wrong |
//! | 2 | the run needs the person: a question of the agent, or an approval nobody gave in time |
//! | 3 | the run was stopped by a limit (the round cap, a cap of the autonomy setting) |
//! | 4 | the run was cancelled (by this client's Ctrl+C or by the person) |
//! | 5 | the core could not be reached, or is too busy to take the run |

use super::http::{safe_id, Client, Frame, NetError, StreamStop};
use super::render::{plain, Renderer};
use super::{Format, Io};
use serde_json::{json, Value};
use std::io::Write;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

pub const EXIT_COMPLETED: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_NEEDS_YOU: i32 = 2;
pub const EXIT_LIMIT: i32 = 3;
pub const EXIT_CANCELLED: i32 = 4;
pub const EXIT_UNREACHABLE: i32 = 5;

/// A stream that sends nothing for this long (the core sends a heartbeat every ten seconds) is taken for a broken connection.
const DEAD_AFTER: Duration = Duration::from_secs(45);
/// How long one read waits before the client looks at its own flags (Ctrl+C).
pub const LOOK_EVERY: Duration = Duration::from_millis(500);
/// How many times in a row the stream may fail to give anything before the client gives up (exit 5).
const MAX_ATTEMPTS: u32 = 8;
/// After the first Ctrl+C the run is given this long to end before the client leaves.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// What the events of a run add up to, for the exit code.
#[derive(Debug, Default, Clone)]
pub struct Outcome {
    /// The `status` of `run.finished`.
    pub status: Option<String>,
    pub stopped_reason: String,
    /// A cap ended the run (`limit` event).
    pub limit: bool,
    /// Questions of the host that nobody answered in time.
    pub approvals_unanswered: u32,
    /// The planner asked the person something.
    pub asked: bool,
    pub error_code: Option<String>,
}

impl Outcome {
    pub fn observe(&mut self, event: &Value) {
        match event["type"].as_str().unwrap_or("") {
            "run.finished" => {
                self.status = event["status"].as_str().map(str::to_string);
                self.stopped_reason = event["stopped_reason"].as_str().unwrap_or("").to_string();
            }
            "limit" => self.limit = true,
            "approval.closed" if event["outcome"].as_str() == Some("timeout") => self.approvals_unanswered += 1,
            "question" => self.asked = true,
            "status" if event["state"].as_str() == Some("waiting_user") => self.asked = true,
            "error" => self.error_code = event["code"].as_str().map(str::to_string),
            _ => {}
        }
    }

    fn capped(&self) -> bool {
        self.limit || self.stopped_reason.starts_with("max_") || self.stopped_reason.contains("limit")
    }

    /// The exit code the events come to. The order matters: a run the person (or Ctrl+C) cancelled is 4 whatever else it said; a cap is 3; a run that waits for the person, or that
    /// could not do something because nobody answered, is 2 even when the model went on and finished; a completed run is 0; everything else is 1.
    pub fn exit_code(&self) -> i32 {
        let status = self.status.as_deref().unwrap_or("");
        if status == "cancelled" {
            return EXIT_CANCELLED;
        }
        if self.capped() {
            return EXIT_LIMIT;
        }
        if status == "waiting_user" || self.approvals_unanswered > 0 || (self.asked && status != "completed") {
            return EXIT_NEEDS_YOU;
        }
        if status == "completed" {
            return EXIT_COMPLETED;
        }
        EXIT_FAILED
    }

    /// One sentence on a code that is not 0, for the error stream of a person.
    pub fn hint(&self) -> Option<&'static str> {
        match self.exit_code() {
            EXIT_NEEDS_YOU => Some("the run needs you: open the Aqua app to answer (the command line cannot approve anything); start it again with --wait-approval SECS to leave the question open longer"),
            EXIT_LIMIT => Some("the run was stopped by a limit before it finished"),
            EXIT_CANCELLED => Some("the run was cancelled"),
            EXIT_FAILED => Some("the run did not complete"),
            _ => None,
        }
    }
}

/// The body of `POST /api/v1/runs`.
pub struct StartArgs {
    pub goal: String,
    pub directory: Option<String>,
    pub max_steps: Option<u32>,
    pub profile: Option<String>,
    pub wait_approval: u64,
    pub preapproval: Option<String>,
    pub idempotency_key: Option<String>,
    /// The session the run is put in (card OA10-04): with `AQUA_CODING_SESSIONS=on` the core tells a coding run of the session what the earlier ones came to.
    pub session_id: Option<String>,
}

impl StartArgs {
    pub fn body(&self) -> Value {
        // A question of the host is open at least ten seconds (the shortest the core allows) and at most ten minutes. 0 means "do not wait for long".
        let mut b = json!({ "goal": self.goal, "approval_wait_secs": self.wait_approval.clamp(10, 600) });
        if let Some(d) = &self.directory {
            b["directory"] = json!(d);
        }
        if let Some(n) = self.max_steps {
            b["max_steps"] = json!(n);
        }
        if let Some(p) = &self.profile {
            b["profile"] = json!(p);
        }
        if let Some(p) = &self.preapproval {
            b["preapproval_id"] = json!(p);
        }
        if let Some(k) = &self.idempotency_key {
            b["idempotency_key"] = json!(k);
        }
        if let Some(n) = &self.session_id {
            b["session_id"] = json!(n);
        }
        b
    }
}

fn say(w: &mut dyn Write, text: &str) {
    let _ = writeln!(w, "{text}");
    let _ = w.flush();
}

/// What a failed request is said to be, and its exit code: a core that is not there or is too busy is 5, anything else is 1.
pub fn http_failure(io: &mut Io, what: &str, status: u16, error: &str, code: &str) -> i32 {
    say(io.err, &format!("{what}: {} ({status}{})", plain(error), if code.is_empty() { String::new() } else { format!(", {code}") }));
    match (status, code) {
        (401, _) => say(io.err, "the API token was refused: is --data the data folder of the Aqua that is running, and was the token file made by it?"),
        (_, "busy" | "not_ready" | "limit_reached") => return EXIT_UNREACHABLE,
        _ => {}
    }
    if matches!(status, 429 | 503) {
        EXIT_UNREACHABLE
    } else {
        EXIT_FAILED
    }
}

pub fn net_failure(io: &mut Io, e: &NetError) -> i32 {
    say(io.err, &plain(e.message()));
    EXIT_UNREACHABLE
}

/// What a watcher wants done next, looked at between two reads of the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wish {
    /// Go on.
    Go,
    /// Ask the core to end the run (once), and wait for it for ten seconds.
    Interrupt,
    /// Ask the core to end the run (once) and leave at once.
    Leave,
}

/// Who follows a run: it is told each frame of the stream, is asked what it wants between two reads, and is given the sentences about the connection. The same loop serves every watcher;
/// they differ in what they do with an event, not in how a broken stream is taken up again.
pub trait Watcher {
    /// One frame that is not a heartbeat and was not seen before (an `end` frame too), in order, with its data read as JSON when it is.
    fn frame(&mut self, frame: &Frame, event: Option<&Value>);
    /// Looked at between two reads (about every `look_every`) and between two frames.
    fn wish(&mut self) -> Wish;
    /// A sentence for the person about the connection ("stopping...", "taking it up again").
    fn note(&mut self, text: &str);
}

/// Why [`watch`] came back without the end of the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// The watcher left: a second Ctrl+C, the grace after the first one ran out, or a wish to leave while the client waited to try again.
    Left,
    /// The core could not be reached after the tries that are made: the last error.
    Net(NetError),
    /// The core would not give the stream.
    Refused { status: u16, message: String, code: String },
    /// The stream kept closing before the run ended.
    KeepsClosing,
}

/// Where a run is followed, and how.
pub struct Watch<'a> {
    /// The client that reads the stream.
    pub stream: &'a Client,
    /// The client that ends the run (the same one, or one whose token may end it).
    pub control: &'a Client,
    /// The unit of the wait before the stream is taken up again.
    pub backoff_unit: Duration,
    /// How long one read waits before the watcher is asked what it wants.
    pub look_every: Duration,
}

/// Sleeps `total`, in short steps, and says whether the watcher wants to leave meanwhile.
fn pause(watcher: &mut dyn Watcher, total: Duration) -> bool {
    let end = Instant::now() + total;
    while Instant::now() < end {
        if watcher.wish() == Wish::Leave {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20).min(end.saturating_duration_since(Instant::now())));
    }
    false
}

/// What the watcher wants: the first wish to stop ends the run (the core stops it at its next step), a wish to leave leaves at once, and so does a run that is still there when the grace
/// is over.
fn look(cfg: &Watch, id: &str, watcher: &mut dyn Watcher, sent: &mut bool, deadline: &mut Option<Instant>) -> Result<(), Stop> {
    let wish = watcher.wish();
    if wish != Wish::Go && !*sent {
        *sent = true;
        *deadline = Some(Instant::now() + STOP_GRACE);
        watcher.note("stopping...");
        let _ = cfg.control.send("POST", &format!("/api/v1/runs/{id}/interrupt"), None);
    }
    if wish == Wish::Leave || deadline.is_some_and(|d| Instant::now() > d) {
        watcher.note("not waiting any more: the run was told to stop");
        return Err(Stop::Left);
    }
    Ok(())
}

/// Follows run `id` from event `after` to the `end` frame of its stream. A stream that breaks is taken up again from the last event that was seen (`?after=`), so nothing is missed and
/// nothing is shown twice; after eight tries in a row the client gives up. What the events add up to is in the outcome.
pub fn watch(cfg: &Watch, id: &str, after: i64, watcher: &mut dyn Watcher) -> Result<Outcome, Stop> {
    let mut outcome = Outcome::default();
    let mut last = after;
    let mut attempts = 0u32;
    let mut sent_interrupt = false;
    let mut stop_deadline: Option<Instant> = None;
    loop {
        look(cfg, id, watcher, &mut sent_interrupt, &mut stop_deadline)?;
        let path = format!("/api/v1/runs/{id}/stream?after={last}");
        match cfg.stream.open_stream(&path, cfg.look_every) {
            Err(e) => {
                attempts += 1;
                if attempts >= MAX_ATTEMPTS {
                    return Err(Stop::Net(e));
                }
                watcher.note(&format!("the connection to the core was lost; trying again ({attempts} of {MAX_ATTEMPTS})"));
                if pause(watcher, cfg.backoff_unit * 2u32.pow(attempts.min(5))) {
                    return Err(Stop::Left);
                }
                continue;
            }
            Ok(Err(reply)) => {
                let (message, code) = reply.error();
                return Err(Stop::Refused { status: reply.status, message, code });
            }
            Ok(Ok(mut stream)) => {
                let mut quiet_since = Instant::now();
                loop {
                    look(cfg, id, watcher, &mut sent_interrupt, &mut stop_deadline)?;
                    match stream.next_frame() {
                        Ok(frame) => {
                            quiet_since = Instant::now();
                            attempts = 0;
                            let data: Option<Value> = serde_json::from_str(&frame.data).ok();
                            if frame.event == "heartbeat" {
                                continue;
                            }
                            // An event the client has seen already (a replay after a reconnect) is not shown twice.
                            let seq = frame.id.as_deref().and_then(|i| i.trim().parse::<i64>().ok());
                            if let Some(n) = seq {
                                if n <= last {
                                    continue;
                                }
                                last = n;
                            }
                            if let Some(ev) = &data {
                                outcome.observe(ev);
                            }
                            watcher.frame(&frame, data.as_ref());
                            if frame.event == "end" {
                                return Ok(outcome);
                            }
                        }
                        Err(StreamStop::Quiet) => {
                            if quiet_since.elapsed() > DEAD_AFTER {
                                break;
                            }
                        }
                        Err(StreamStop::Closed | StreamStop::Broken(_)) => break,
                    }
                }
                attempts += 1;
                if attempts >= MAX_ATTEMPTS {
                    watcher.note("the stream of the run keeps closing before the run ended");
                    return Err(Stop::KeepsClosing);
                }
                watcher.note(&format!("the stream was cut off after event {last}; taking it up again ({attempts} of {MAX_ATTEMPTS})"));
                if pause(watcher, cfg.backoff_unit * 2u32.pow(attempts.min(5))) {
                    return Err(Stop::Left);
                }
            }
        }
    }
}

/// Runs `aqua-cli run`: starts the run, and (unless `--detach`) follows it. The exit code.
pub fn run(client: &Client, args: &StartArgs, format: Format, detach: bool, io: &mut Io) -> i32 {
    let reply = match client.send("POST", "/api/v1/runs", Some(&args.body())) {
        Ok(r) => r,
        Err(e) => return net_failure(io, &e),
    };
    if !(reply.status == 200 || reply.status == 202) {
        let (msg, code) = reply.error();
        return http_failure(io, "the run was not started", reply.status, &msg, &code);
    }
    let started = reply.json();
    let Some(id) = started["run_id"].as_str().filter(|i| safe_id(i)).map(str::to_string) else {
        say(io.err, "the core's answer to the start of a run did not name the run");
        return EXIT_FAILED;
    };
    if detach {
        match format {
            Format::Json => say(io.out, &started.to_string()),
            Format::Text => say(io.out, &format!("started {id} ({}, place {})\nfollow it with: aqua-cli runs events {id} --follow", started["state"].as_str().unwrap_or("queued"), started["position"])),
        }
        return EXIT_COMPLETED;
    }
    if format == Format::Text {
        if let Some(n) = started["position"].as_u64().filter(|n| *n > 0) {
            say(io.err, &format!("the core is busy: the run waits for its turn (place {n})"));
        }
    }
    follow(client, &id, 0, format, io)
}

/// The watcher of `aqua-cli run` and `runs events --follow`: the events as lines of text, or as the JSON the core sent; the Ctrl+C count is the wish.
struct TextWatcher<'a, 'b> {
    io: &'a mut Io<'b>,
    renderer: Renderer,
    format: Format,
}

impl Watcher for TextWatcher<'_, '_> {
    fn frame(&mut self, frame: &Frame, event: Option<&Value>) {
        if self.format == Format::Json {
            say(self.io.out, &frame.data);
        }
        if let Some(ev) = event {
            if self.format == Format::Text && frame.event != "end" {
                for line in self.renderer.lines(ev) {
                    say(self.io.out, &line);
                }
            }
        }
    }

    fn wish(&mut self) -> Wish {
        match self.io.interrupts.load(Ordering::SeqCst) {
            0 => Wish::Go,
            1 => Wish::Interrupt,
            _ => Wish::Leave,
        }
    }

    fn note(&mut self, text: &str) {
        say(self.io.err, text);
    }
}

/// Follows run `id` from event `after` to its end and gives the exit code (see the table at the top of this file).
pub fn follow(client: &Client, id: &str, after: i64, format: Format, io: &mut Io) -> i32 {
    let cfg = Watch { stream: client, control: client, backoff_unit: io.backoff_unit, look_every: LOOK_EVERY };
    let result = {
        let renderer = Renderer::new(io.color && format == Format::Text);
        let mut watcher = TextWatcher { io: &mut *io, renderer, format };
        watch(&cfg, id, after, &mut watcher)
    };
    match result {
        Ok(outcome) => finish(client, id, outcome, io),
        Err(Stop::Left) => EXIT_CANCELLED,
        Err(Stop::Net(e)) => net_failure(io, &e),
        Err(Stop::Refused { status, message, code }) => http_failure(io, "the run could not be followed", status, &message, &code),
        Err(Stop::KeepsClosing) => EXIT_UNREACHABLE,
    }
}

/// A run whose stream ended without a `run.finished` (the run was lost) is asked once what it came to.
pub fn resolve_outcome(client: &Client, id: &str, outcome: &mut Outcome) {
    if outcome.status.is_none() {
        if let Ok(r) = client.send("GET", &format!("/api/v1/runs/{id}/wait?timeout=0"), None) {
            let v = r.json();
            outcome.status = v["status"].as_str().map(str::to_string);
            outcome.stopped_reason = v["stopped_reason"].as_str().unwrap_or("").to_string();
        }
    }
}

/// The end of the stream: the exit code of what was seen.
fn finish(client: &Client, id: &str, mut outcome: Outcome, io: &mut Io) -> i32 {
    resolve_outcome(client, id, &mut outcome);
    let code = outcome.exit_code();
    if let Some(hint) = outcome.hint() {
        say(io.err, hint);
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(events: &[Value]) -> Outcome {
        let mut o = Outcome::default();
        for e in events {
            o.observe(e);
        }
        o
    }

    fn finished(status: &str, reason: &str) -> Value {
        json!({ "type": "run.finished", "status": status, "stopped_reason": reason })
    }

    #[test]
    fn the_exit_code_follows_how_the_run_ended() {
        assert_eq!(outcome(&[finished("completed", "final")]).exit_code(), EXIT_COMPLETED);
        assert_eq!(outcome(&[finished("failed", "aborted")]).exit_code(), EXIT_FAILED);
        assert_eq!(outcome(&[finished("interrupted", "process_ended")]).exit_code(), EXIT_FAILED);
        assert_eq!(outcome(&[finished("cancelled", "cancelled")]).exit_code(), EXIT_CANCELLED);
        assert_eq!(outcome(&[finished("failed", "max_rounds")]).exit_code(), EXIT_LIMIT);
        assert_eq!(outcome(&[json!({ "type": "limit", "stopped_by": "max_tool_calls", "used": 9, "cap": 8 }), finished("failed", "final")]).exit_code(), EXIT_LIMIT);
        assert_eq!(outcome(&[json!({ "type": "question", "text": "", "choices": [] }), finished("waiting_user", "waiting_user")]).exit_code(), EXIT_NEEDS_YOU);
        assert_eq!(outcome(&[json!({ "type": "approval.closed", "request_id": "a", "outcome": "timeout" }), finished("completed", "final")]).exit_code(), EXIT_NEEDS_YOU, "a question nobody answered is the person's");
        assert_eq!(outcome(&[json!({ "type": "approval.closed", "request_id": "a", "outcome": "denied" }), finished("completed", "final")]).exit_code(), EXIT_COMPLETED, "a no is an answer");
        assert_eq!(outcome(&[finished("cancelled", "cancelled"), json!({ "type": "limit", "stopped_by": "max_seconds", "used": 1, "cap": 1 })]).exit_code(), EXIT_CANCELLED, "cancelled wins");
        assert_eq!(Outcome::default().exit_code(), EXIT_FAILED, "a run that said nothing did not complete");
    }

    #[test]
    fn a_code_that_is_not_zero_has_a_sentence() {
        assert!(outcome(&[finished("completed", "final")]).hint().is_none());
        for (events, word) in [(vec![finished("waiting_user", "waiting_user")], "needs you"), (vec![finished("failed", "max_rounds")], "limit"), (vec![finished("cancelled", "x")], "cancelled"), (vec![finished("failed", "x")], "did not complete")] {
            assert!(outcome(&events).hint().unwrap().contains(word), "{word}");
        }
    }

    #[test]
    fn the_wait_for_an_approval_is_kept_between_ten_seconds_and_ten_minutes() {
        let args = |w| StartArgs { goal: "g".into(), directory: None, max_steps: None, profile: None, wait_approval: w, preapproval: None, idempotency_key: None, session_id: None }.body();
        assert_eq!(args(0)["approval_wait_secs"], 10);
        assert_eq!(args(90)["approval_wait_secs"], 90);
        assert_eq!(args(100_000)["approval_wait_secs"], 600);
        let full = StartArgs { goal: "g".into(), directory: Some("E:/p".into()), max_steps: Some(24), profile: Some("coding".into()), wait_approval: 30, preapproval: Some("pa_1".into()), idempotency_key: Some("k".into()), session_id: Some("night.1".into()) }.body();
        assert_eq!((full["directory"].as_str(), full["max_steps"].as_u64(), full["profile"].as_str(), full["preapproval_id"].as_str(), full["idempotency_key"].as_str()), (Some("E:/p"), Some(24), Some("coding"), Some("pa_1"), Some("k")));
        assert_eq!(full["session_id"], "night.1");
    }
}

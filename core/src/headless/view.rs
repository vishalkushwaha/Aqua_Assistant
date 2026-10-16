//! `aqua-cli tui`: a terminal view of a run (card OA10-07; task UX-06 of the OpenCode audit; decision D-09 taken as the audit recommends).
//!
//! ```text
//! aqua-cli tui "<goal>" [--dir PATH] [--session NAME] [--profile P] [--max-steps N] [--wait-approval SECS] [--preapproval ID] [--ui] [--width N] [--no-color]
//! aqua-cli tui --attach RUN_ID [--ui]          follow a run that is going (or replay one that is over)
//! aqua-cli tui                                 at a terminal: ask for goals one after another, in one session
//! ```
//!
//! It is the same client as `aqua-cli run` (the stream is followed, and taken up again after a break, by `run::watch`), shown for a person at a terminal who wants to see what the agent
//! does: the steps, each tool call as a row, the output of a call cut to a few lines, the diff of a change that waits for a yes, a question of the agent, a status line (`[running | 0:12 |
//! ctx 61% (3.7k/6.1k) | 8.4 tok/s | ttft 1.2 s]`) after each reply of the model, and at the end what the run came to. It is **line-oriented on purpose**: it writes lines and never moves the cursor,
//! takes no raw mode and no alternate screen (there is no state to restore after a panic, and the scrollback and the copy of the terminal work as they always did), so it needs no crate
//! (`ratatui` and `crossterm` are still in the manifest of `aqua-cli` and nothing uses them: the old placeholder screen was deleted, card OA13-04). Colour only on a terminal and not with `NO_COLOR`; without colour the marks of a diff (`+`, `-`, `@@`)
//! are the text, so nothing is lost.
//!
//! **It never answers anything.** An approval and a question of the agent are answered in the Aqua app, with the app's token. With the API token (the default) a run says only that it
//! waits; with `--ui` the view reads the stream of the run with the app's own token **for reading only** (it shows what is asked and the diff of the change, and what a call was given
//! and returned), says so on the screen when it starts, needs a person at a terminal, and has no word that answers, approves or changes anything. The run is started, ended and steered
//! with the API token in every case. While a run goes, a typed line is said to it (`/steer TEXT`, or just the text), `/queue TEXT` keeps it for after, `/stop` ends the run, and the first
//! Ctrl+C asks the core to end it too; at the prompt Ctrl+C leaves.
//!
//! Everything a run says is text that a model, a file or a program wrote: every control character is replaced before it is drawn, an invisible or direction-changing character is shown by
//! its number, and a line is cut to the width of the terminal (a block of code or a diff is not re-flowed, only clipped at 300 characters).

use super::http::{core_address, read_token, safe_id, Client, Frame, TokenKind};
use super::run::{http_failure, net_failure, resolve_outcome, watch, StartArgs, Stop, Watch, Watcher, Wish, EXIT_CANCELLED, EXIT_FAILED, EXIT_UNREACHABLE, LOOK_EVERY};
use super::{fail, parse, say, whole_path, Io, COMMON};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

/// The width when nothing says otherwise (`--width`, then `COLUMNS`, then this).
pub const DEFAULT_WIDTH: usize = 100;
/// Below this a line gets no indent and the status line is short.
pub const NARROW: usize = 60;
/// A block of code or of a diff is clipped, not wrapped, at this many characters.
const MAX_CODE_LINE: usize = 300;
/// How many lines of what a call returned are shown when it failed, and when it did not.
const FAILED_OUTPUT_LINES: usize = 10;
const OK_OUTPUT_LINES: usize = 3;
/// How many lines of a diff are shown: the core makes at most 120, so this only guards a core that made more.
const DIFF_LINES: usize = 120;
/// How long a typed line may be.
const MAX_TYPED: usize = 1_000;
/// How long the prompt waits for a line before it looks at Ctrl+C again.
const PROMPT_LOOK: Duration = Duration::from_millis(100);
/// How many sessions `/sessions` lists.
const SESSIONS_SHOWN: u32 = 10;

/// How lines are drawn.
#[derive(Clone, Copy, Debug)]
pub struct Look {
    /// ANSI colour codes (a terminal, and not `NO_COLOR`).
    pub color: bool,
    /// Columns of the terminal (20 to 300).
    pub width: usize,
    /// The view reads with the app's token: questions and diffs can be shown.
    pub ui: bool,
}

impl Look {
    fn narrow(&self) -> bool {
        self.width < NARROW
    }
}

/// Draws one text for a terminal: control characters (the escape character, the bell, a carriage return, the C1 range) become `?`, a tab one space, and a character that cannot be seen or
/// that changes the direction of the text is shown by its number (`<U+202E>`), so that a text cannot draw itself as something else.
pub fn safe(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\t' => out.push(' '),
            c if c.is_control() => out.push('?'),
            c if invisible(c) => out.push_str(&format!("<U+{:04X}>", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn invisible(c: char) -> bool {
    matches!(c as u32, 0x00AD | 0x061C | 0x180E | 0x200B..=0x200F | 0x2028..=0x202E | 0x2060..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB | 0xE0000..=0xE007F)
}

/// `text` cut to `max` characters, with `...` when something was left out.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}...", text.chars().take(max.saturating_sub(3)).collect::<String>())
    }
}

/// One paragraph as lines of at most `width` characters: `first` before the first line and `rest` before the others; a word longer than a line is split.
pub fn wrap(text: &str, width: usize, first: &str, rest: &str) -> Vec<String> {
    let width = width.max(20);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = first.to_string();
    let mut len = first.chars().count();
    let mut has_word = false;
    for word in text.split_whitespace() {
        let wl = word.chars().count();
        let need = wl + usize::from(has_word);
        if len + need <= width {
            if has_word {
                cur.push(' ');
            }
            cur.push_str(word);
            len += need;
            has_word = true;
            continue;
        }
        if has_word {
            lines.push(std::mem::replace(&mut cur, rest.to_string()));
            len = rest.chars().count();
        }
        // The word has a line to itself; a word longer than the room is split.
        let chars: Vec<char> = word.chars().collect();
        let mut at = 0;
        loop {
            let room = width.saturating_sub(len).max(1);
            if chars.len() - at <= room {
                break;
            }
            cur.extend(&chars[at..at + room]);
            lines.push(std::mem::replace(&mut cur, rest.to_string()));
            len = rest.chars().count();
            at += room;
        }
        let tail: String = chars[at..].iter().collect();
        len += tail.chars().count();
        cur.push_str(&tail);
        has_word = true;
    }
    if has_word || lines.is_empty() {
        lines.push(cur);
    }
    lines.into_iter().map(|l| l.trim_end().to_string()).collect()
}

fn kilo(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1_000.0)
    }
}

struct Tool {
    name: String,
    /// Lines of its live output shown so far, and lines left out.
    shown: usize,
    left_out: usize,
}

/// Turns the events of one run into lines, one event at a time, and keeps what the status line says.
pub struct View {
    look: Look,
    tools: HashMap<i64, Tool>,
    last_title: String,
    /// A `status` said that a question is open and its `approval.opened` has not come.
    awaiting: bool,
    approval_open: bool,
    state: String,
    first_ts: Option<i64>,
    last_ts: i64,
    ctx: Option<(u64, u64)>,
    tps: Option<f64>,
    ttft_ms: Option<u64>,
    /// Inside a fenced block of the answer: whether it is a diff.
    fence: Option<bool>,
    last_status: String,
}

impl View {
    pub fn new(look: Look) -> Self {
        Self { look, tools: HashMap::new(), last_title: String::new(), awaiting: false, approval_open: false, state: "starting".into(), first_ts: None, last_ts: 0, ctx: None, tps: None, ttft_ms: None, fence: None, last_status: String::new() }
    }

    fn paint(&self, code: &str, text: String) -> String {
        if self.look.color && !code.is_empty() {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text
        }
    }

    /// A paragraph: wrapped, safe, and drawn in `code`.
    fn para(&self, text: &str, code: &str, first: &str, rest: &str) -> Vec<String> {
        wrap(&safe(text), self.look.width, first, rest).into_iter().map(|l| self.paint(code, l)).collect()
    }

    /// The indent of a line under a heading.
    fn indent(&self) -> &'static str {
        if self.look.narrow() {
            " "
        } else {
            "  "
        }
    }

    /// One line of a diff, drawn: `+` green, `-` red, `@@` cyan, the two file lines bold. Without colour the line is the text it is.
    fn diff_line(&self, line: &str) -> String {
        let line = clip(&safe(line), MAX_CODE_LINE);
        let code = if line.starts_with("+++") || line.starts_with("---") {
            "1"
        } else if line.starts_with("@@") {
            "36"
        } else if line.starts_with('+') {
            "32"
        } else if line.starts_with('-') {
            "31"
        } else if line.starts_with('\u{2026}') {
            "2"
        } else {
            ""
        };
        self.paint(code, line)
    }

    /// The diff of a change that waits for a yes, as lines (the first `DIFF_LINES`, and a line that says how many were left out).
    fn diff_block(&self, preview: &str, indent: &str) -> Vec<String> {
        let all: Vec<&str> = preview.lines().collect();
        let mut out: Vec<String> = all.iter().take(DIFF_LINES).map(|l| format!("{indent}{}", self.diff_line(l))).collect();
        if all.len() > DIFF_LINES {
            out.push(self.paint("2", format!("{indent}... {} more lines not shown here (the app shows them)", all.len() - DIFF_LINES)));
        }
        out
    }

    fn generic_approval(&self) -> Vec<String> {
        let hint = if self.look.ui { "Answer it in the Aqua app: this terminal cannot approve anything" } else { "Answer it in the Aqua app (this terminal cannot approve anything; `aqua-cli tui --ui` shows what is asked)" };
        self.para(&format!("approval needed: the run waits for a yes or a no. {hint}."), "33", "! ", "  ")
    }

    /// The status line: `[running | 0:12 | ctx 61% (3.7k/6.1k) | 8.4 tok/s | ttft 1.2 s]`, shorter in a narrow terminal.
    pub fn status_line(&self) -> String {
        let secs = if self.last_ts > 0 { self.first_ts.map_or(0, |f| ((self.last_ts - f).max(0) / 1000) as u64) } else { 0 };
        let mut parts = vec![self.state.clone(), format!("{}:{:02}", secs / 60, secs % 60)];
        if let Some((used, window)) = self.ctx.filter(|(_, w)| *w > 0) {
            parts.push(if self.look.narrow() { format!("ctx {}%", used * 100 / window) } else { format!("ctx {}% ({}/{})", used * 100 / window, kilo(used), kilo(window)) });
        }
        if !self.look.narrow() {
            if let Some(t) = self.tps {
                parts.push(format!("{t:.1} tok/s"));
            }
            if let Some(ms) = self.ttft_ms {
                parts.push(format!("ttft {:.1} s", ms as f64 / 1000.0));
            }
        }
        clip(&format!("[{}]", parts.join(" | ")), self.look.width)
    }

    /// The status line as drawn, when it is not the one that was drawn last.
    fn status_if_new(&mut self) -> Option<String> {
        let line = self.status_line();
        (line != self.last_status).then(|| {
            self.last_status = line.clone();
            self.paint("2", line)
        })
    }

    /// The lines for one event (none for the ones that have nothing to say).
    pub fn lines(&mut self, ev: &Value) -> Vec<String> {
        let kind = ev["type"].as_str().unwrap_or("");
        if let Some(ts) = ev["ts"].as_i64() {
            self.first_ts.get_or_insert(ts);
            self.last_ts = ts;
        }
        let mut out = Vec::new();
        // A question was said to be open, and what it asks never came (the API token's stream says no more): say the little that is known, before whatever comes next.
        if self.awaiting && !matches!(kind, "approval.opened" | "status") {
            self.awaiting = false;
            self.approval_open = true;
            out.extend(self.generic_approval());
        }
        let s = |key: &str| safe(ev[key].as_str().unwrap_or(""));
        let ind = self.indent();
        match kind {
            "run.started" => {
                out.extend(self.para(&format!("run {}: {}", s("run_id"), s("goal_head")), "1", "> ", "  "));
                if let Some(dir) = ev["directory"].as_str().filter(|d| !d.is_empty()) {
                    out.extend(self.para(&format!("folder: {}", safe(dir)), "", ind, ind));
                }
                self.state = "running".into();
            }
            "status" => {
                let state = ev["state"].as_str().unwrap_or("");
                match state {
                    "queued" => {
                        let place = ev["position"].as_u64().map(|p| format!(" (place {p} in the line)")).unwrap_or_default();
                        out.push(format!("... waiting for its turn on the model{place}"));
                    }
                    "waiting_approval" => {
                        if !self.approval_open {
                            self.awaiting = true;
                        }
                    }
                    "waiting_user" => {
                        // The question itself is the `question` event's; this says only that the run ended on one.
                    }
                    "running" => {
                        self.approval_open = false;
                        self.awaiting = false;
                    }
                    _ => {}
                }
                if !state.is_empty() {
                    self.state = safe(state);
                }
            }
            "step" => {
                let title = clip(&s("title"), self.look.width * 2);
                match ev["status"].as_str().unwrap_or("") {
                    "failed" => out.extend(self.para(&format!("{title} failed"), "31", "! ", "  ")),
                    "running" if title != self.last_title => {
                        self.last_title = title.clone();
                        out.extend(self.para(&title, "", "> ", "  "));
                    }
                    _ => {}
                }
            }
            "tool.started" => {
                let tool = s("tool");
                if let Some(id) = ev["call_id"].as_i64() {
                    if self.tools.len() > 512 {
                        self.tools.clear();
                    }
                    self.tools.insert(id, Tool { name: tool.clone(), shown: 0, left_out: 0 });
                }
                let args = ev["args_head"].as_str().filter(|a| !a.is_empty()).map(|a| format!(" {}", clip(&safe(a), 160))).unwrap_or_default();
                out.extend(self.para(&format!("{tool}{args}"), "2", "$ ", "  "));
            }
            "tool.progress" => {
                let call = ev["call_id"].as_i64().unwrap_or(-1);
                let slot = self.tools.entry(call).or_insert_with(|| Tool { name: String::new(), shown: 0, left_out: 0 });
                slot.left_out += ev["skipped"].as_u64().unwrap_or(0) as usize;
                let mut shown_now = Vec::new();
                for line in ev["text"].as_str().unwrap_or("").lines() {
                    if slot.shown < FAILED_OUTPUT_LINES {
                        slot.shown += 1;
                        shown_now.push(line.to_string());
                    } else {
                        slot.left_out += 1;
                    }
                }
                for line in shown_now {
                    out.push(format!("{ind}| {}", clip(&safe(&line), self.look.width)));
                }
            }
            "tool.finished" => {
                let call = ev["call_id"].as_i64().unwrap_or(-1);
                let tool = self.tools.get(&call);
                let (name, shown, left_out) = (tool.map_or_else(|| "a tool".to_string(), |t| if t.name.is_empty() { "a tool".to_string() } else { t.name.clone() }), tool.map_or(0, |t| t.shown), tool.map_or(0, |t| t.left_out));
                let ms = ev["ms"].as_i64().unwrap_or(0);
                let ok = ev["ok"].as_bool() != Some(false);
                if !ok {
                    out.extend(self.para(&format!("{name} {} ({} in {ms} ms)", s("status"), s("decision")), "31", &format!("{ind}! "), &format!("{ind}  ")));
                } else if !s("approval").is_empty() {
                    out.extend(self.para(&format!("{name} ok in {ms} ms ({})", s("approval")), "", ind, ind));
                }
                // What the call returned (the app's token only): the live output was shown already when there was any.
                let mut more = left_out;
                if shown == 0 {
                    let all: Vec<&str> = ev["observation_head"].as_str().unwrap_or("").lines().collect();
                    let keep = if ok { OK_OUTPUT_LINES } else { FAILED_OUTPUT_LINES };
                    for line in all.iter().take(keep) {
                        out.push(format!("{ind}| {}", clip(&safe(line), self.look.width)));
                    }
                    more = all.len().saturating_sub(keep);
                }
                if more > 0 {
                    out.push(self.paint("2", format!("{ind}| ... {more} more lines")));
                }
                self.tools.remove(&call);
            }
            "approval.opened" => {
                self.awaiting = false;
                if std::mem::replace(&mut self.approval_open, true) {
                    return out;
                }
                let request = &ev["request"];
                let asked = request.as_object().is_some_and(|r| !r.is_empty());
                if !(self.look.ui && asked) {
                    out.extend(self.generic_approval());
                    return out;
                }
                let r = |key: &str| safe(request[key].as_str().unwrap_or(""));
                let what = if r("reason").is_empty() { r("tool") } else { format!("{} ({})", r("reason"), r("tool")) };
                out.extend(self.para(&format!("approval needed: {what}"), "1;33", "! ", "  "));
                for (label, key) in [("risk", "risk"), ("rule", "rule")] {
                    if !r(key).is_empty() {
                        out.extend(self.para(&format!("{label}: {}", r(key)), "", ind, ind));
                    }
                }
                if let Some(data) = request["data"].as_array().filter(|d| !d.is_empty()) {
                    let items: Vec<String> = data.iter().take(8).filter_map(Value::as_str).map(safe).collect();
                    out.extend(self.para(&format!("involves: {}", items.join(", ")), "", ind, ind));
                }
                if let Some(preview) = request["preview"].as_str().filter(|p| !p.is_empty()) {
                    out.push(format!("{ind}what it would change:"));
                    out.extend(self.diff_block(preview, &format!("{ind}{ind}")));
                }
                if let Some(secs) = request["expires_in_secs"].as_u64() {
                    out.extend(self.para(&format!("it is open for {secs} s more"), "2", ind, ind));
                }
                out.extend(self.para("answer it in the Aqua app: this terminal cannot approve anything", "33", ind, ind));
            }
            "approval.closed" => {
                self.approval_open = false;
                out.push(format!("{ind}approval {}", s("outcome")));
            }
            "question" => {
                let asked = ev["text"].as_str().filter(|t| !t.is_empty());
                match (self.look.ui, asked) {
                    (true, Some(text)) => {
                        out.extend(self.para(&format!("the agent asks: {}", safe(text)), "1;33", "? ", "  "));
                        if let Some(choices) = ev["choices"].as_array() {
                            for (i, c) in choices.iter().take(9).enumerate() {
                                let label = c["label"].as_str().or_else(|| c.as_str()).unwrap_or("");
                                out.extend(self.para(&format!("{}) {}", i + 1, safe(label)), "", ind, &format!("{ind}   ")));
                            }
                        }
                        out.extend(self.para("answer it in the Aqua app: this terminal cannot answer a question", "33", ind, ind));
                    }
                    _ => out.extend(self.para("the agent asked you something: answer it in the Aqua app (this terminal cannot answer a question)", "33", "? ", "  ")),
                }
            }
            "text" => {
                for line in ev["text"].as_str().unwrap_or("").lines() {
                    let trimmed = line.trim_start();
                    if trimmed.starts_with("```") {
                        let opening = self.fence.is_none();
                        self.fence = if opening { Some(trimmed.trim_start_matches('`').trim().eq_ignore_ascii_case("diff")) } else { None };
                        out.push(self.paint("2", clip(&safe(line), MAX_CODE_LINE)));
                    } else if let Some(is_diff) = self.fence {
                        out.push(if is_diff { self.diff_line(line) } else { clip(&safe(line), MAX_CODE_LINE) });
                    } else {
                        out.extend(wrap(&safe(line), self.look.width, "", ""));
                    }
                }
            }
            "usage" => {
                if let (Some(used), Some(window)) = (ev["ctx_used"].as_u64(), ev["ctx_window"].as_u64()) {
                    self.ctx = Some((used, window));
                }
                if let Some(t) = ev["decode_tps"].as_f64() {
                    self.tps = Some(t);
                }
                if let Some(ms) = ev["ttft_ms"].as_u64() {
                    self.ttft_ms = Some(ms);
                }
                out.extend(self.status_if_new());
            }
            "limit" => out.extend(self.para(&format!("stopped by a limit: {} ({} of {})", s("stopped_by"), ev["used"], ev["cap"]), "33", "! ", "  ")),
            "error" => out.extend(self.para(&format!("error {}: {}", s("code"), clip(&s("message"), self.look.width * 3)), "31", "! ", "  ")),
            "run.finished" => {
                let status = s("status");
                self.state = status.clone();
                let secs = ev["ms"].as_f64().unwrap_or(0.0) / 1000.0;
                let line = format!("= {status} ({}) in {secs:.1} s, {} files changed", s("stopped_reason"), ev["files_changed"].as_u64().unwrap_or(0));
                out.push(self.paint(if status == "completed" { "32" } else { "31" }, clip(&line, self.look.width)));
                if let Some(why) = ev["why_it_stopped"].as_str().filter(|w| !w.is_empty()) {
                    out.extend(self.para(why, "", ind, ind));
                }
            }
            _ => {}
        }
        out
    }
}

/// One line on what a run came to, from the `summary` of `GET /api/v1/runs/{id}/transcript`: calls, files, tokens, questions.
pub fn summary_line(transcript: &Value) -> Option<String> {
    let s = transcript.get("summary").filter(|s| s.is_object())?;
    let n = |key: &str| s[key].as_u64().unwrap_or(0);
    let mut parts = vec![format!("{} tool calls{}", n("tool_calls"), if n("tool_errors") > 0 { format!(" ({} failed)", n("tool_errors")) } else { String::new() })];
    if let Some(files) = s["files_changed"].as_u64() {
        parts.push(format!("{files} files changed"));
    }
    if n("tokens_in") + n("tokens_out") > 0 {
        parts.push(format!("tokens {} in / {} out", kilo(n("tokens_in")), kilo(n("tokens_out"))));
    }
    let q = &s["questions"];
    let asked = q["asked"].as_u64().unwrap_or(0);
    if asked > 0 {
        parts.push(format!("{asked} questions asked ({} approved, {} denied, {} not answered)", q["approved"].as_u64().unwrap_or(0), q["denied"].as_u64().unwrap_or(0), q["timed_out"].as_u64().unwrap_or(0)));
    }
    match s["last_check"]["ok"].as_bool() {
        Some(true) => parts.push("the last check passed".into()),
        Some(false) => parts.push("the last check failed".into()),
        None => {}
    }
    Some(format!("  {}", safe(&parts.join(", "))))
}

/// The lines of help for the words typed at the prompt and while a run goes.
const PROMPT_HELP: [&str; 6] = [
    "type a goal and press Enter to start a run in this session",
    "/new          start another session (the next run does not hear of this one)",
    "/sessions     the sessions of this token, newest first",
    "/runs         the runs that are going",
    "/help         this list",
    "/quit         leave (Ctrl+C at the prompt does too)",
];
const RUN_HELP: [&str; 5] = [
    "a typed line is said to the run, for its next step (or /steer TEXT)",
    "/queue TEXT   keep it for after this run",
    "/stop         end the run (the first Ctrl+C does too, the second leaves at once)",
    "/status       the status line now",
    "the run's questions and approvals are answered in the Aqua app, never here",
];

/// What the view works with, for every run of one command.
struct Ctx {
    api: Client,
    /// The app's token, for reading (`--ui`).
    reader: Option<Client>,
    look: Look,
    dir: Option<String>,
    profile: String,
    max_steps: Option<u32>,
    wait: u64,
    preapproval: Option<String>,
    session: Option<String>,
}

/// The watcher of the view: the lines of each event, the typed lines, and the Ctrl+C count.
struct ViewWatcher<'a, 'b, 'c> {
    io: &'a mut Io<'b>,
    view: View,
    typed: Option<&'c Receiver<String>>,
    control: &'c Client,
    run_id: String,
    /// The Ctrl+C count when this run began: earlier presses belong to earlier runs.
    base: usize,
    stop_wanted: bool,
}

impl ViewWatcher<'_, '_, '_> {
    /// Says a line to the run (`steer`: at its next step; `queue`: after it).
    fn tell(&mut self, text: &str, queue: bool) {
        let text: String = text.trim().chars().take(MAX_TYPED).collect();
        if text.is_empty() {
            say(self.io.err, "say what to tell the run: /steer TEXT or /queue TEXT");
            return;
        }
        let body = json!({ "text": text, "delivery": if queue { "queue" } else { "steer" } });
        match self.control.send("POST", &format!("/api/v1/runs/{}/messages", self.run_id), Some(&body)) {
            Ok(r) if (200..300).contains(&r.status) => {
                let note = r.json()["note"].as_str().unwrap_or(if queue { "kept for after the run" } else { "told to the run" }).to_string();
                say(self.io.out, &format!("  -> {}", safe(&note)));
            }
            Ok(r) => {
                let (message, code) = r.error();
                say(self.io.err, &format!("the run was not told: {} ({}{})", safe(&message), r.status, if code.is_empty() { String::new() } else { format!(", {code}") }));
            }
            Err(e) => say(self.io.err, &safe(e.message())),
        }
    }

    fn typed_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let Some(command) = line.strip_prefix('/') else {
            self.tell(line, false);
            return;
        };
        let (word, rest) = command.split_once(char::is_whitespace).unwrap_or((command, ""));
        match word {
            "stop" => {
                self.stop_wanted = true;
            }
            "steer" => self.tell(rest, false),
            "queue" => self.tell(rest, true),
            "status" => {
                let line = self.view.status_line();
                say(self.io.out, &line);
            }
            "help" => {
                for l in RUN_HELP {
                    say(self.io.out, l);
                }
            }
            other => say(self.io.err, &format!("/{} is not a word while a run goes: /steer, /queue, /stop, /status, /help", safe(other))),
        }
    }
}

impl Watcher for ViewWatcher<'_, '_, '_> {
    fn frame(&mut self, frame: &Frame, event: Option<&Value>) {
        if frame.event == "end" {
            return;
        }
        if let Some(ev) = event {
            for line in self.view.lines(ev) {
                say(self.io.out, &line);
            }
        }
    }

    fn wish(&mut self) -> Wish {
        if let Some(rx) = self.typed {
            while let Ok(line) = rx.try_recv() {
                self.typed_line(&line);
            }
        }
        match self.io.interrupts.load(Ordering::SeqCst).saturating_sub(self.base) {
            0 if !self.stop_wanted => Wish::Go,
            0 | 1 => Wish::Interrupt,
            _ => Wish::Leave,
        }
    }

    fn note(&mut self, text: &str) {
        say(self.io.err, &safe(text));
    }
}

/// The exit code of a run that was followed in the view, and what it came to.
fn follow_view(ctx: &Ctx, id: &str, after: i64, io: &mut Io, typed: Option<&Receiver<String>>) -> i32 {
    let reader = ctx.reader.as_ref().unwrap_or(&ctx.api);
    let cfg = Watch { stream: reader, control: &ctx.api, backoff_unit: io.backoff_unit, look_every: LOOK_EVERY };
    let base = io.interrupts.load(Ordering::SeqCst);
    let result = {
        let mut watcher = ViewWatcher { io: &mut *io, view: View::new(ctx.look), typed, control: &ctx.api, run_id: id.to_string(), base, stop_wanted: false };
        watch(&cfg, id, after, &mut watcher)
    };
    match result {
        Ok(mut outcome) => {
            resolve_outcome(&ctx.api, id, &mut outcome);
            if let Ok(r) = ctx.api.send("GET", &format!("/api/v1/runs/{id}/transcript"), None) {
                if r.status == 200 {
                    if let Some(line) = summary_line(&r.json()) {
                        say(io.out, &line);
                    }
                }
            }
            if let Some(hint) = outcome.hint() {
                say(io.err, hint);
            }
            outcome.exit_code()
        }
        Err(Stop::Left) => EXIT_CANCELLED,
        Err(Stop::Net(e)) => net_failure(io, &e),
        Err(Stop::Refused { status, message, code }) => http_failure(io, "the run could not be followed", status, &message, &code),
        Err(Stop::KeepsClosing) => EXIT_UNREACHABLE,
    }
}

/// Starts a run for `goal`: the id of the run, or the exit code of the failure.
fn start(ctx: &Ctx, goal: &str, io: &mut Io) -> Result<String, i32> {
    let args = StartArgs {
        goal: goal.to_string(),
        directory: ctx.dir.clone(),
        max_steps: ctx.max_steps,
        profile: Some(ctx.profile.clone()),
        wait_approval: ctx.wait,
        preapproval: ctx.preapproval.clone(),
        idempotency_key: None,
        session_id: ctx.session.clone(),
    };
    let reply = match ctx.api.send("POST", "/api/v1/runs", Some(&args.body())) {
        Ok(r) => r,
        Err(e) => return Err(net_failure(io, &e)),
    };
    if !(reply.status == 200 || reply.status == 202) {
        let (msg, code) = reply.error();
        return Err(http_failure(io, "the run was not started", reply.status, &msg, &code));
    }
    let started = reply.json();
    let Some(id) = started["run_id"].as_str().filter(|i| safe_id(i)).map(str::to_string) else {
        say(io.err, "the core's answer to the start of a run did not name the run");
        return Err(EXIT_FAILED);
    };
    if let Some(n) = started["position"].as_u64().filter(|n| *n > 0) {
        say(io.err, &format!("the core is busy: the run waits for its turn (place {n})"));
    }
    Ok(id)
}

/// A new name for a session of this view: letters, digits and `_`, at most 64.
fn new_session_name() -> String {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    format!("tui_{ms:x}_{:x}", std::process::id())
}

/// What the prompt hears: a line, or that the person is leaving (Ctrl+C, the end of the input).
fn next_line(io: &mut Io, typed: &Receiver<String>, base: usize) -> Option<String> {
    loop {
        if io.interrupts.load(Ordering::SeqCst) > base {
            return None;
        }
        match typed.recv_timeout(PROMPT_LOOK) {
            Ok(line) => return Some(line),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

/// The prompt: goals one after another in one session, until the person leaves.
fn prompt_loop(ctx: &mut Ctx, io: &mut Io, typed: &Receiver<String>) -> i32 {
    ctx.session.get_or_insert_with(new_session_name);
    say(io.err, "Type a goal and press Enter. /help lists the words; Ctrl+C or /quit leaves. Questions and approvals are answered in the Aqua app, never here.");
    loop {
        if io.stdin_is_terminal {
            let _ = std::io::Write::write_all(io.out, b"aqua> ");
            let _ = std::io::Write::flush(io.out);
        }
        let base = io.interrupts.load(Ordering::SeqCst);
        let Some(line) = next_line(io, typed, base) else { return 0 };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if let Some(command) = line.strip_prefix('/') {
            let word = command.split_whitespace().next().unwrap_or("");
            match word {
                "quit" | "exit" | "q" => return 0,
                "help" => {
                    for l in PROMPT_HELP {
                        say(io.out, l);
                    }
                }
                "new" => {
                    ctx.session = Some(new_session_name());
                    say(io.out, "a new session");
                }
                "sessions" => list_sessions(ctx, io),
                "runs" => list_runs(ctx, io),
                "stop" => say(io.err, "no run is going from here"),
                other => say(io.err, &format!("/{} is not a word here: /help lists them (a goal that starts with a slash: put a space before it)", safe(other))),
            }
            continue;
        }
        let goal: String = line.chars().take(4_000).collect();
        match start(ctx, &goal, io) {
            Ok(id) => {
                let code = follow_view(ctx, &id, 0, io, Some(typed));
                if code != 0 {
                    say(io.err, &format!("(the run ended with exit code {code})"));
                }
            }
            Err(code) => say(io.err, &format!("(nothing was started: exit code {code})")),
        }
    }
}

fn list_sessions(ctx: &Ctx, io: &mut Io) {
    match ctx.api.send("GET", &format!("/api/v1/sessions?limit={SESSIONS_SHOWN}"), None) {
        Ok(r) if r.status == 200 => {
            let list = r.json()["sessions"].as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                say(io.out, "no session of this token yet");
            }
            for s in list {
                say(io.out, &format!("{}  {} ({} runs){}", safe(s["id"].as_str().unwrap_or("")), clip(&safe(s["title"].as_str().unwrap_or("")), 60), s["runs"], if s["going"] == true { "  going" } else { "" }));
            }
        }
        Ok(r) => {
            let (message, code) = r.error();
            say(io.err, &format!("the sessions could not be listed: {} ({}{})", safe(&message), r.status, if code.is_empty() { String::new() } else { format!(", {code}") }));
        }
        Err(e) => say(io.err, &safe(e.message())),
    }
}

fn list_runs(ctx: &Ctx, io: &mut Io) {
    match ctx.api.send("GET", "/api/v1/runs/active", None) {
        Ok(r) if r.status == 200 => {
            let runs = r.json()["runs"].as_array().cloned().unwrap_or_default();
            if runs.is_empty() {
                say(io.out, "no run of this token is going");
            }
            for run in runs {
                say(io.out, &format!("{}  {}  {}", safe(run["run_id"].as_str().unwrap_or("")), safe(run["state"].as_str().unwrap_or("")), clip(&safe(run["goal"].as_str().unwrap_or("")), 80)));
            }
        }
        Ok(r) => {
            let (message, _) = r.error();
            say(io.err, &format!("the runs could not be listed: {} ({})", safe(&message), r.status));
        }
        Err(e) => say(io.err, &safe(e.message())),
    }
}

/// The width: `--width`, else `COLUMNS`, else 100; never below 20 or above 300.
fn width_of(given: Option<&str>) -> Result<usize, String> {
    let n = match given {
        Some(g) => g.trim().parse::<usize>().map_err(|_| "--width is a whole number of columns".to_string())?,
        None => std::env::var("COLUMNS").ok().and_then(|c| c.trim().parse::<usize>().ok()).unwrap_or(DEFAULT_WIDTH),
    };
    Ok(n.clamp(20, 300))
}

/// `aqua-cli tui`: the words are in the header of this file. `typed` is what the person types while a run goes and at the prompt (the program reads standard input in a thread; a test gives a
/// channel; `None` means that nobody can type). The exit code is the run's (see `run.rs`), or 0 for the prompt.
pub fn run(args: &[String], default_data: &Path, io: &mut Io, typed: Option<&Receiver<String>>) -> i32 {
    let rest = &args[1..];
    let with: Vec<&str> = COMMON.iter().copied().filter(|c| *c != "format").chain(["dir", "wait-approval", "max-steps", "profile", "preapproval", "session", "attach", "width"]).collect();
    let p = match parse(rest, &with, &["ui", "no-color"]) {
        Ok(p) => p,
        Err(e) => return fail(io, &format!("tui: {e}")),
    };
    let mut goal = p.rest.join(" ").trim().to_string();
    if let Some(piped) = io.stdin_text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        goal = if goal.is_empty() { piped.to_string() } else { format!("{goal}\n\n{piped}") };
    }
    let attach = p.value("attach").map(str::to_string);
    if attach.is_some() && !goal.is_empty() {
        return fail(io, "tui: --attach follows a run that exists: it takes no goal");
    }
    if attach.as_deref().is_some_and(|i| !safe_id(i)) {
        return fail(io, "tui: --attach is the id of a run");
    }
    let number = |name: &str| -> Result<Option<u64>, String> { p.value(name).map(|v| v.trim().parse::<u64>().map_err(|_| format!("--{name} is a whole number"))).transpose() };
    let (wait, steps) = match (number("wait-approval"), number("max-steps")) {
        (Ok(w), Ok(s)) => (w.unwrap_or(120), s),
        (Err(e), _) | (_, Err(e)) => return fail(io, &format!("tui: {e}")),
    };
    let width = match width_of(p.value("width")) {
        Ok(w) => w,
        Err(e) => return fail(io, &format!("tui: {e}")),
    };
    let preapproval = p.value("preapproval").map(str::to_string);
    if preapproval.as_deref().is_some_and(|i| !safe_id(i)) {
        return fail(io, "tui: --preapproval is the id that `aqua-cli preapprove` printed");
    }
    let session = p.value("session").map(str::to_string);
    if session.as_deref().is_some_and(|n| n.is_empty() || n.len() > 64 || !n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))) {
        return fail(io, "tui: --session is a name of 1 to 64 letters, digits and _ . : -");
    }
    let dir = p.value("dir").map(whole_path);
    if preapproval.is_some() && dir.is_none() {
        return fail(io, "tui: a pre-approval is for one folder: say which with --dir");
    }
    let data_folder: PathBuf = p.value("data-dir").or(p.value("data")).map(PathBuf::from).unwrap_or_else(|| default_data.to_path_buf());
    let addr = match core_address(p.value("core")) {
        Ok(a) => a,
        Err(e) => return fail(io, &format!("tui: {e}")),
    };
    let api = match read_token(TokenKind::Api, p.value("token-file").map(PathBuf::from).as_deref(), &data_folder) {
        Ok(t) => Client::new(addr, t),
        Err(e) => return fail(io, &format!("tui: {e}")),
    };
    // `--ui`: the app's own token, for reading what a run is asked and shows (a diff, a question), by a person at a terminal who has been told so, and nothing else.
    let reader = if p.flag("ui") {
        if !io.stdin_is_terminal {
            return fail(io, "tui --ui: it reads with the app's own token and needs a person at a terminal; nothing was done");
        }
        if p.value("token-file").is_some() {
            return fail(io, "tui --ui: the app's token is read from the data folder only");
        }
        match read_token(TokenKind::Ui, None, &data_folder) {
            Ok(t) => Some(Client::new(addr, t)),
            Err(e) => return fail(io, &format!("tui --ui: {e}")),
        }
    } else {
        None
    };
    let color = io.color && !p.flag("no-color");
    let mut ctx = Ctx { api, reader, look: Look { color, width, ui: p.flag("ui") }, dir, profile: p.value("profile").unwrap_or("coding").to_string(), max_steps: steps.map(|s| s.min(u32::MAX as u64) as u32), wait, preapproval, session };
    if ctx.reader.is_some() {
        say(io.err, "--ui: this view reads with the app's own token (read only): it shows what a call is asked and gives, and the diff of a change. It cannot answer, approve or change anything: do that in the Aqua app.");
    }
    if let Some(id) = attach {
        return follow_view(&ctx, &id, 0, io, typed);
    }
    if !goal.is_empty() {
        let goal: String = goal.chars().take(4_000).collect();
        return match start(&ctx, &goal, io) {
            Ok(id) => follow_view(&ctx, &id, 0, io, typed),
            Err(code) => code,
        };
    }
    match typed {
        Some(rx) if io.stdin_is_terminal => prompt_loop(&mut ctx, io, rx),
        _ => fail(io, "usage: aqua-cli tui \"<goal>\" [--dir PATH] [--session NAME] [--ui] | tui --attach RUN_ID | tui (at a terminal: asks for goals)"),
    }
}

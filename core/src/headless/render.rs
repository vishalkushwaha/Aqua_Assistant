//! The text form of a run's events, for a person at a terminal (card OA10-05; task UX-05 of the OpenCode audit).
//!
//! `> step`, `$ tool`, sentences of the answer, `!` for what needs the person and `=` for how it ended. Colour only when the caller says so (a terminal); never in `--format json`.
//!
//! Everything a run says is text a model or a program wrote. Before it is drawn, every control character is replaced (an escape sequence in a tool's output must not be able to move
//! the cursor, clear the screen or change the title of the window), and one event is at most a few lines.

use serde_json::Value;
use std::collections::HashMap;

/// The longest a line of arguments or an answer piece may be on the screen.
const MAX_LINE: usize = 400;

/// Draws one text for a terminal: control characters (the escape character, the bell, a carriage return, the C1 range) become `?`; tabs become one space.
pub fn plain(text: &str) -> String {
    text.chars().map(|c| if c == '\t' { ' ' } else if c.is_control() { '?' } else { c }).collect()
}

fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}...", text.chars().take(max).collect::<String>())
    }
}

/// Turns the events of one run into lines, one event at a time.
pub struct Renderer {
    color: bool,
    tools: HashMap<i64, String>,
    last_title: String,
    approval_open: bool,
}

impl Renderer {
    pub fn new(color: bool) -> Self {
        Self { color, tools: HashMap::new(), last_title: String::new(), approval_open: false }
    }

    fn paint(&self, code: &str, text: String) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text
        }
    }

    /// The lines for one event (none for the ones that have nothing to say: the usage numbers, a phase that ended well).
    pub fn lines(&mut self, event: &Value) -> Vec<String> {
        let s = |key: &str| plain(event[key].as_str().unwrap_or(""));
        match event["type"].as_str().unwrap_or("") {
            "run.started" => vec![self.paint("1", format!("> run {}: {}", s("run_id"), cut(&s("goal_head"), MAX_LINE)))],
            "status" => match event["state"].as_str().unwrap_or("") {
                "queued" => {
                    let place = event["position"].as_u64().map(|p| format!(" (place {p} in the line)")).unwrap_or_default();
                    vec![format!("... waiting for its turn on the model{place}")]
                }
                "waiting_approval" => {
                    if std::mem::replace(&mut self.approval_open, true) {
                        Vec::new()
                    } else {
                        vec![self.paint("33", "! approval needed in the Aqua app: the run waits for a yes or a no there".to_string())]
                    }
                }
                "waiting_user" => vec![self.paint("33", "! the agent asked you something: answer it in the Aqua app".to_string())],
                "running" => {
                    self.approval_open = false;
                    Vec::new()
                }
                other => vec![format!("... {}", plain(other))],
            },
            "step" => {
                let title = cut(&s("title"), MAX_LINE);
                match event["status"].as_str().unwrap_or("") {
                    "failed" => vec![self.paint("31", format!("! {title} failed"))],
                    "running" if title != self.last_title => {
                        self.last_title = title.clone();
                        vec![format!("> {title}")]
                    }
                    _ => Vec::new(),
                }
            }
            "tool.started" => {
                let tool = s("tool");
                if let Some(id) = event["call_id"].as_i64() {
                    if self.tools.len() > 512 {
                        self.tools.clear();
                    }
                    self.tools.insert(id, tool.clone());
                }
                let args = event["args_head"].as_str().filter(|a| !a.is_empty()).map(|a| format!(" {}", cut(&plain(a), 160))).unwrap_or_default();
                vec![self.paint("2", format!("$ {tool}{args}"))]
            }
            "tool.finished" => {
                let tool = event["call_id"].as_i64().and_then(|id| self.tools.get(&id).cloned()).unwrap_or_else(|| "a tool".to_string());
                let ms = event["ms"].as_i64().unwrap_or(0);
                if event["ok"].as_bool() == Some(false) {
                    vec![self.paint("31", format!("  ! {tool} {} ({} in {ms} ms)", s("status"), s("decision")))]
                } else if !s("approval").is_empty() {
                    vec![format!("  {tool} ok in {ms} ms ({})", s("approval"))]
                } else {
                    Vec::new()
                }
            }
            "tool.progress" => cut(&s("text"), 2000).lines().map(|l| format!("  | {l}")).collect(),
            "approval.opened" => {
                if std::mem::replace(&mut self.approval_open, true) {
                    return Vec::new();
                }
                // The question itself is the UI token's; a client with the API token is told only that one is open.
                let said = event["request"]["summary"].as_str().map(|m| format!(": {}", cut(&plain(m), MAX_LINE))).unwrap_or_default();
                vec![self.paint("33", format!("! approval needed in the Aqua app{said}"))]
            }
            "approval.closed" => {
                self.approval_open = false;
                vec![format!("  approval {}", s("outcome"))]
            }
            "text" => cut(&s("text"), MAX_LINE * 2).lines().map(str::to_string).collect(),
            "limit" => vec![self.paint("33", format!("! stopped by a limit: {} ({} of {})", s("stopped_by"), event["used"], event["cap"]))],
            "question" => vec![self.paint("33", "! the agent asked you something: answer it in the Aqua app".to_string())],
            "error" => vec![self.paint("31", format!("! error {}: {}", s("code"), cut(&s("message"), MAX_LINE)))],
            "run.finished" => {
                let secs = event["ms"].as_f64().unwrap_or(0.0) / 1000.0;
                let status = s("status");
                let line = format!("= {status} ({}) in {secs:.1} s, {} files changed", s("stopped_reason"), event["files_changed"].as_u64().unwrap_or(0));
                let mut out = vec![self.paint(if status == "completed" { "32" } else { "31" }, line)];
                if let Some(why) = event["why_it_stopped"].as_str().filter(|w| !w.is_empty()) {
                    out.push(format!("  {}", cut(&plain(why), MAX_LINE)));
                }
                out
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_control_character_in_what_a_run_says_is_never_drawn() {
        assert_eq!(plain("a\x1b[2Jb\x07c\rd\te\u{9b}f"), "a?[2Jb?c?d e?f");
        let mut r = Renderer::new(false);
        let lines = r.lines(&json!({ "type": "text", "index": 1, "text": "done\x1b]0;owned\x07" }));
        assert_eq!(lines, vec!["done?]0;owned?".to_string()]);
        assert!(lines.iter().all(|l| !l.contains('\x1b')));
    }

    #[test]
    fn the_events_of_a_run_read_as_steps_tools_sentences_and_an_end() {
        let mut r = Renderer::new(false);
        let mut all = Vec::new();
        for e in [
            json!({ "type": "run.started", "run_id": "run_1", "goal_head": "add a doc comment" }),
            json!({ "type": "step", "step": 1, "phase": "plan", "title": "Plan with llama.cpp", "status": "running" }),
            json!({ "type": "step", "step": 1, "phase": "plan", "title": "Plan with llama.cpp", "status": "completed" }),
            json!({ "type": "tool.started", "call_id": 3, "tool": "fs.read" }),
            json!({ "type": "tool.finished", "call_id": 3, "ok": true, "status": "completed", "ms": 12, "decision": "allowed", "approval": "" }),
            json!({ "type": "tool.started", "call_id": 4, "tool": "term.run" }),
            json!({ "type": "tool.finished", "call_id": 4, "ok": false, "status": "denied", "ms": 1, "decision": "denied", "approval": "denied" }),
            json!({ "type": "usage", "tokens_in": 10 }),
            json!({ "type": "text", "index": 1, "text": "I added it." }),
            json!({ "type": "run.finished", "status": "completed", "stopped_reason": "final", "answer_head": "I added it.", "files_changed": 1, "ms": 2500 }),
        ] {
            all.extend(r.lines(&e));
        }
        assert_eq!(
            all,
            vec![
                "> run run_1: add a doc comment",
                "> Plan with llama.cpp",
                "$ fs.read",
                "$ term.run",
                "  ! term.run denied (denied in 1 ms)",
                "I added it.",
                "= completed (final) in 2.5 s, 1 files changed",
            ]
        );
    }

    #[test]
    fn a_run_that_waits_for_an_approval_says_so_once_and_never_what_the_question_is_unless_it_was_told() {
        let mut r = Renderer::new(false);
        let first = r.lines(&json!({ "type": "status", "state": "waiting_approval" }));
        let again = r.lines(&json!({ "type": "approval.opened", "request": {} }));
        assert_eq!(first.len(), 1);
        assert!(first[0].starts_with("! approval needed in the Aqua app"));
        assert!(again.is_empty(), "one line per question");
        r.lines(&json!({ "type": "approval.closed", "request_id": "a", "outcome": "timeout" }));
        let ui = r.lines(&json!({ "type": "approval.opened", "request": { "summary": "Run `cargo test` in E:/x" } }));
        assert_eq!(ui, vec!["! approval needed in the Aqua app: Run `cargo test` in E:/x".to_string()]);
    }

    #[test]
    fn colour_is_added_only_when_asked_for() {
        let e = json!({ "type": "error", "code": "internal", "message": "boom" });
        assert_eq!(Renderer::new(false).lines(&e), vec!["! error internal: boom".to_string()]);
        assert_eq!(Renderer::new(true).lines(&e), vec!["\x1b[31m! error internal: boom\x1b[0m".to_string()]);
    }
}

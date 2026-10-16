//! The terminal view (card OA10-07; task UX-06 of the OpenCode audit): what the events of a run look like as lines (snapshots of the transcript, a diff, an approval, a question, the narrow
//! layout), and what the command asks of the core (the stub of `tests.rs`: no model, no real core).

use super::*;
use crate::headless::view::{self, safe, summary_line, wrap, Look, View};
use std::sync::mpsc::{channel, Receiver};

fn look(ui: bool, color: bool, width: usize) -> Look {
    Look { color, width, ui }
}

/// An event as the stream carries it, with the time it was written.
fn ev(kind: &str, ts: i64, mut fields: Value) -> Value {
    fields["type"] = json!(kind);
    fields["ts"] = json!(ts);
    fields
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            for d in chars.by_ref() {
                if d == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn lines_of(view: &mut View, events: &[Value]) -> Vec<String> {
    events.iter().flat_map(|e| view.lines(e)).collect()
}

const PREVIEW: &str = "--- E:/work/a.txt\n+++ E:/work/a.txt\n@@ -2,5 +2,5 @@\n two\n three\n-four\n+FOUR\n five\n six";

fn approval_events() -> Vec<Value> {
    vec![
        ev("tool.started", 1_000, json!({ "call_id": 3, "tool": "fs.edit", "args_head": "{\"path\":\"E:/work/a.txt\"}" })),
        ev("status", 1_010, json!({ "state": "waiting_approval" })),
        ev(
            "approval.opened",
            1_020,
            json!({ "request": { "request_id": "c_1", "tool": "fs.edit", "reason": "Change a file: E:/work/a.txt", "risk": "It changes a file on this computer.", "rule": "every edit asks", "data": ["E:/work/a.txt"], "expires_in_secs": 120, "preview": PREVIEW } }),
        ),
        ev("approval.closed", 5_000, json!({ "request_id": "c_1", "outcome": "approved" })),
        ev("status", 5_010, json!({ "state": "running" })),
        ev("tool.finished", 5_100, json!({ "call_id": 3, "ok": true, "status": "completed", "ms": 80, "decision": "asked", "approval": "approved" })),
    ]
}

#[test]
fn a_run_reads_as_steps_tool_rows_a_status_line_the_answer_and_an_end() {
    let mut view = View::new(look(false, false, 80));
    let events = [
        ev("run.started", 1_000, json!({ "run_id": "run_1", "goal_head": "Fix the parser", "directory": "D:/proj" })),
        ev("status", 1_001, json!({ "state": "running" })),
        ev("step", 1_002, json!({ "step": 1, "phase": "plan", "title": "Plan with llama.cpp", "status": "running" })),
        ev("step", 1_500, json!({ "step": 1, "phase": "plan", "title": "Plan with llama.cpp", "status": "completed" })),
        ev("tool.started", 1_600, json!({ "call_id": 1, "tool": "fs.read" })),
        ev("tool.finished", 1_700, json!({ "call_id": 1, "ok": true, "status": "completed", "ms": 12, "decision": "allowed", "approval": "" })),
        ev("tool.started", 1_800, json!({ "call_id": 2, "tool": "check.run" })),
        ev("tool.finished", 3_800, json!({ "call_id": 2, "ok": false, "status": "failed", "ms": 2000, "decision": "allowed", "approval": "" })),
        ev("usage", 4_000, json!({ "tokens_in": 1000, "tokens_out": 80, "ttft_ms": 1200, "decode_tps": 8.4, "ctx_used": 3700, "ctx_window": 6144 })),
        ev("text", 13_000, json!({ "index": 1, "text": "I fixed the parser." })),
        ev("run.finished", 13_000, json!({ "status": "completed", "stopped_reason": "final", "answer_head": "I fixed the parser.", "files_changed": 1, "ms": 12000 })),
    ];
    let lines = lines_of(&mut view, &events);
    assert_eq!(
        lines,
        vec![
            "> run run_1: Fix the parser",
            "  folder: D:/proj",
            "> Plan with llama.cpp",
            "$ fs.read",
            "$ check.run",
            "  ! check.run failed (allowed in 2000 ms)",
            "[running | 0:03 | ctx 60% (3.7k/6.1k) | 8.4 tok/s | ttft 1.2 s]",
            "I fixed the parser.",
            "= completed (final) in 12.0 s, 1 files changed",
        ]
    );
}

#[test]
fn the_status_line_has_the_state_the_time_the_context_the_speed_and_the_first_token_and_is_short_in_a_narrow_terminal() {
    let events = [
        ev("run.started", 1_000, json!({ "run_id": "r", "goal_head": "g" })),
        ev("usage", 13_000, json!({ "ttft_ms": 1200, "decode_tps": 8.4, "ctx_used": 3700, "ctx_window": 6144 })),
    ];
    let mut wide = View::new(look(false, false, 100));
    lines_of(&mut wide, &events);
    assert_eq!(wide.status_line(), "[running | 0:12 | ctx 60% (3.7k/6.1k) | 8.4 tok/s | ttft 1.2 s]");
    let mut narrow = View::new(look(false, false, 40));
    lines_of(&mut narrow, &events);
    assert_eq!(narrow.status_line(), "[running | 0:12 | ctx 60%]");
    // A status line is drawn once for the same numbers, not after every reply.
    assert!(wide.lines(&ev("usage", 13_100, json!({ "ctx_used": 3700, "ctx_window": 6144 }))).is_empty());
    // Before anything is known there is still a line that says so.
    assert_eq!(View::new(look(false, false, 100)).status_line(), "[starting | 0:00]");
}

#[test]
fn the_diff_of_a_change_that_waits_is_shown_to_the_app_token_and_the_marks_are_the_text_without_colour() {
    let mut view = View::new(look(true, false, 80));
    let lines = lines_of(&mut view, &approval_events());
    assert_eq!(
        lines,
        vec![
            "$ fs.edit {\"path\":\"E:/work/a.txt\"}",
            "! approval needed: Change a file: E:/work/a.txt (fs.edit)",
            "  risk: It changes a file on this computer.",
            "  rule: every edit asks",
            "  involves: E:/work/a.txt",
            "  what it would change:",
            "    --- E:/work/a.txt",
            "    +++ E:/work/a.txt",
            "    @@ -2,5 +2,5 @@",
            "     two",
            "     three",
            "    -four",
            "    +FOUR",
            "     five",
            "     six",
            "  it is open for 120 s more",
            "  answer it in the Aqua app: this terminal cannot approve anything",
            "  approval approved",
            "  fs.edit ok in 80 ms (approved)",
        ]
    );
}

#[test]
fn with_colour_the_added_and_removed_lines_are_painted_and_the_text_under_the_paint_is_the_same() {
    let mut plain = View::new(look(true, false, 80));
    let mut coloured = View::new(look(true, true, 80));
    let (a, b) = (lines_of(&mut plain, &approval_events()), lines_of(&mut coloured, &approval_events()));
    assert_eq!(a, b.iter().map(|l| strip_ansi(l)).collect::<Vec<_>>(), "colour changes no word");
    let added = b.iter().find(|l| l.contains("+FOUR")).unwrap();
    let removed = b.iter().find(|l| l.contains("-four")).unwrap();
    let hunk = b.iter().find(|l| l.contains("@@ -2,5")).unwrap();
    assert!(added.contains("\x1b[32m") && removed.contains("\x1b[31m") && hunk.contains("\x1b[36m"), "{added:?} {removed:?} {hunk:?}");
}

#[test]
fn the_api_token_is_told_only_that_a_question_is_open_and_the_view_says_how_to_see_it() {
    let mut view = View::new(look(false, false, 200));
    // The API token's stream has the two events and no field: the line is drawn once, whichever comes first, and says what is not known.
    let lines = lines_of(&mut view, &[ev("status", 1, json!({ "state": "waiting_approval" })), ev("approval.opened", 2, json!({ "request": {} })), ev("approval.closed", 3, json!({ "request_id": "c_1", "outcome": "timeout" }))]);
    assert_eq!(
        lines,
        vec![
            "! approval needed: the run waits for a yes or a no. Answer it in the Aqua app (this terminal cannot approve anything; `aqua-cli tui --ui` shows what is asked).",
            "  approval timeout",
        ]
    );
    // A stream that said the run waits and then went on to something else still gets its line before it.
    let mut view = View::new(look(false, false, 200));
    let lines = lines_of(&mut view, &[ev("status", 1, json!({ "state": "waiting_approval" })), ev("run.finished", 2, json!({ "status": "failed", "stopped_reason": "x", "files_changed": 0, "ms": 1 }))]);
    assert!(lines[0].starts_with("! approval needed"), "{lines:?}");
    assert!(lines[1].starts_with("= failed"));
}

#[test]
fn a_question_of_the_agent_is_shown_with_its_choices_to_the_app_token_and_never_answered_here() {
    let question = ev("question", 1, json!({ "text": "Which parser do you mean?", "choices": ["the JSON one", "the TOML one"] }));
    let mut ui = View::new(look(true, false, 100));
    assert_eq!(
        ui.lines(&question),
        vec!["? the agent asks: Which parser do you mean?", "  1) the JSON one", "  2) the TOML one", "  answer it in the Aqua app: this terminal cannot answer a question"]
    );
    let mut api = View::new(look(false, false, 200));
    assert_eq!(api.lines(&ev("question", 1, json!({}))), vec!["? the agent asked you something: answer it in the Aqua app (this terminal cannot answer a question)"]);
}

#[test]
fn what_a_call_returned_is_cut_to_a_few_lines_and_the_rest_is_counted() {
    let mut view = View::new(look(true, false, 80));
    let output: String = (1..=25).map(|i| format!("error line {i}")).collect::<Vec<_>>().join("\n");
    let lines = lines_of(
        &mut view,
        &[
            ev("tool.started", 1, json!({ "call_id": 8, "tool": "check.run", "args_head": "{}" })),
            ev("tool.finished", 2, json!({ "call_id": 8, "ok": false, "status": "failed", "ms": 900, "decision": "allowed", "approval": "", "observation_head": output })),
            ev("tool.started", 3, json!({ "call_id": 9, "tool": "fs.read", "args_head": "{}" })),
            ev("tool.finished", 4, json!({ "call_id": 9, "ok": true, "status": "completed", "ms": 3, "decision": "allowed", "approval": "", "observation_head": "a\nb\nc\nd\ne" })),
        ],
    );
    assert_eq!(lines.iter().filter(|l| l.starts_with("  | error line")).count(), 10, "a failed call shows ten lines");
    assert!(lines.contains(&"  | ... 15 more lines".to_string()), "{lines:?}");
    assert_eq!(lines.iter().filter(|l| matches!(l.as_str(), "  | a" | "  | b" | "  | c")).count(), 3, "a call that went well shows three");
    assert!(lines.contains(&"  | ... 2 more lines".to_string()));
}

#[test]
fn live_output_is_shown_as_it_comes_up_to_a_few_lines_and_the_rest_is_counted_once() {
    let mut view = View::new(look(true, false, 80));
    let mut lines = lines_of(&mut view, &[ev("tool.started", 1, json!({ "call_id": 5, "tool": "term.run", "args_head": "cargo test" }))]);
    lines.extend(view.lines(&ev("tool.progress", 2, json!({ "call_id": 5, "piece": 1, "text": "   Compiling a\n   Compiling b", "skipped": 0 }))));
    let many: String = (0..30).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
    lines.extend(view.lines(&ev("tool.progress", 3, json!({ "call_id": 5, "piece": 2, "text": many, "skipped": 4 }))));
    lines.extend(view.lines(&ev("tool.finished", 4, json!({ "call_id": 5, "ok": true, "status": "completed", "ms": 9000, "decision": "allowed", "approval": "", "observation_head": "test result: ok" }))));
    assert_eq!(lines.iter().filter(|l| l.starts_with("  | ")).count(), 11, "ten lines of output, and the count: {lines:?}");
    assert_eq!(lines.last().unwrap(), "  | ... 26 more lines", "22 of the 32 lines that came, and the 4 that were dropped before they came, counted once at the end of the call: {lines:?}");
    assert!(!lines.iter().any(|l| l.contains("test result")), "what the call returned is not shown again when its output was");
}

#[test]
fn nothing_a_run_says_can_move_the_cursor_or_draw_as_something_else() {
    assert_eq!(safe("a\x1b[2Jb\x07c\rd\te\u{9b}f"), "a?[2Jb?c?d e?f");
    assert_eq!(safe("exe\u{202e}txt.cod\u{200b}e\u{feff}"), "exe<U+202E>txt.cod<U+200B>e<U+FEFF>");
    let mut view = View::new(look(true, false, 80));
    let hostile = lines_of(
        &mut view,
        &[
            ev("run.started", 1, json!({ "run_id": "r\x1b]0;x\x07", "goal_head": "g\x1b[2J" })),
            ev("tool.started", 2, json!({ "call_id": 1, "tool": "fs.edit\x1b[31m", "args_head": "{\"p\":\"\u{202e}x\"}" })),
            ev("approval.opened", 3, json!({ "request": { "tool": "fs.edit", "reason": "r\x1b[2J", "preview": "--- a\n+++ a\n@@ -1 +1 @@\n-x\n+\x1b[2Jevil\u{202e}" } })),
            ev("text", 4, json!({ "text": "```diff\n+\x1b[2Jadded\u{202e}\n```\nbye\x1b[2J" })),
        ],
    );
    for line in &hostile {
        assert!(!line.contains('\x1b') && !line.contains('\u{202e}') && !line.contains('\r'), "{line:?}");
    }
    assert!(hostile.iter().any(|l| l.contains("+?[2Jevil<U+202E>")), "{hostile:?}");
}

#[test]
fn a_line_is_wrapped_to_the_width_a_long_word_is_split_and_a_block_of_code_is_clipped_not_reflowed() {
    assert_eq!(wrap("aaa bbb ccc ddd eee fff", 20, "> ", "  "), vec!["> aaa bbb ccc ddd", "  eee fff"]);
    // The narrowest width is 20: a word longer than a line is split, and no line is longer than the width.
    let long = wrap(&format!("see {}", "x".repeat(50)), 20, "! ", "  ");
    assert!(long.iter().all(|l| l.chars().count() <= 20), "{long:?}");
    assert_eq!(long.concat().replace(' ', "").matches('x').count(), 50, "nothing is lost");
    assert_eq!(wrap("", 20, "! ", "  "), vec!["!"]);
    // Code in the answer is not re-flowed; a diff in the answer is painted line by line.
    let mut view = View::new(look(false, false, 30));
    let code = "x".repeat(100);
    let lines = view.lines(&ev("text", 1, json!({ "text": format!("Here it is, a rather long sentence that must wrap.\n```rust\nlet a = \"{code}\";\n```") })));
    assert_eq!(lines[0], "Here it is, a rather long");
    assert!(lines.iter().any(|l| l.starts_with("let a = ") && l.chars().count() > 100), "{lines:?}");
}

#[test]
fn the_narrow_layout_has_one_space_of_indent_and_still_says_everything() {
    let mut view = View::new(look(true, false, 40));
    let lines = lines_of(&mut view, &approval_events());
    let is_diff = |l: &String| l.trim_start().starts_with(['+', '-', '@']) || l.trim_start().starts_with("two") || l.trim_start().starts_with("three") || l.trim_start().starts_with("five") || l.trim_start().starts_with("six");
    assert!(lines.iter().filter(|l| !is_diff(l)).all(|l| l.chars().count() <= 40), "{lines:?}");
    assert!(lines.contains(&" what it would change:".to_string()), "{lines:?}");
    assert!(lines.contains(&"  -four".to_string()) && lines.contains(&"  +FOUR".to_string()), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("answer it in the Aqua app")) && lines.iter().any(|l| l.contains("it is open for 120 s more")));
}

#[test]
fn the_summary_of_a_finished_run_is_one_line_of_what_it_did() {
    let transcript = json!({ "summary": { "status": "completed", "tool_calls": 12, "tool_errors": 1, "files_changed": 3, "tokens_in": 3700, "tokens_out": 640, "questions": { "asked": 2, "approved": 1, "denied": 1, "timed_out": 0 }, "last_check": { "tool": "check.run", "ok": true } } });
    assert_eq!(summary_line(&transcript).unwrap(), "  12 tool calls (1 failed), 3 files changed, tokens 3.7k in / 640 out, 2 questions asked (1 approved, 1 denied, 0 not answered), the last check passed");
    assert_eq!(summary_line(&json!({ "summary": { "tool_calls": 0, "questions": { "asked": 0 } } })).unwrap(), "  0 tool calls");
    assert!(summary_line(&json!({ "error": "no" })).is_none());
}

// ---- the command ---------------------------------------------------------------------------------------------------------------------------------------------------------------

fn run_with(stub_url: &str, data: &Path, args: &[&str], typed: Option<&Receiver<String>>, interrupts: &AtomicUsize, tweak: impl FnOnce(&mut Io)) -> Ran {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let confirm = |_: &str| false;
    let code = {
        let mut io = Io { out: &mut out, err: &mut err, stdin_text: None, stdin_is_terminal: false, color: false, interrupts, backoff_unit: Duration::from_millis(5), openapi: "{}", confirm: &confirm };
        tweak(&mut io);
        let mut full: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        full.extend(["--core".into(), stub_url.to_string(), "--data-dir".into(), data.to_string_lossy().to_string()]);
        view::run(&full, data, &mut io, typed)
    };
    Ran { code, out: String::from_utf8_lossy(&out).to_string(), err: String::from_utf8_lossy(&err).to_string() }
}

#[test]
fn the_command_starts_a_coding_run_follows_it_and_ends_with_the_exit_code_of_the_run() {
    let data = data_dir("tui_run");
    for (frames, want) in [
        (vec![row(1, started()), row(2, json!({ "type": "text", "index": 1, "text": "Done." })), row(3, finished("completed", "final")), end_frame(3)], 0),
        (vec![row(1, started()), row(2, finished("failed", "aborted")), end_frame(2)], 1),
        (vec![row(1, started()), row(2, json!({ "type": "question", "text": "", "choices": [] })), row(3, finished("waiting_user", "waiting_user")), end_frame(3)], 2),
        (vec![row(1, started()), row(2, finished("failed", "max_rounds")), end_frame(2)], 3),
        (vec![row(1, started()), row(2, finished("cancelled", "cancelled")), end_frame(2)], 4),
    ] {
        let stub = stub_with_stream(frames.clone());
        let ran = go(&stub.url(), &data, &["tui", "fix", "the", "parser", "--dir", "E:/p", "--max-steps", "12"], |_| {});
        assert_eq!(ran.code, want, "{frames:?}\nout: {}\nerr: {}", ran.out, ran.err);
        let body: Value = serde_json::from_str(&stub.requests()[0].body).unwrap();
        assert_eq!(body["goal"], "fix the parser");
        assert_eq!((body["profile"].as_str(), body["approval_wait_secs"].as_u64(), body["max_steps"].as_u64()), (Some("coding"), Some(120), Some(12)), "a terminal view is for coding, and a person who is at the screen has time to open the app");
        assert!(body["directory"].as_str().unwrap().replace('\\', "/").ends_with("E:/p"));
        assert!(body.get("session_id").is_none(), "one goal is not a session");
        assert!(ran.out.contains("> run run_stub_1"), "{}", ran.out);
    }
}

#[test]
fn it_reads_the_summary_of_a_finished_run_from_the_transcript_route() {
    let data = data_dir("tui_summary");
    let stub = Stub::start(|req, _| {
        if req.method == "POST" && req.path == "/api/v1/runs" {
            return start_answer();
        }
        if req.path.starts_with("/api/v1/runs/run_stub_1/stream") {
            return Reply::Stream(Box::new(|c| {
                chunk(c, &row(1, started()));
                chunk(c, &row(2, finished("completed", "final")));
                chunk(c, &end_frame(2));
            }));
        }
        if req.path == "/api/v1/runs/run_stub_1/transcript" {
            return Reply::Json(200, json!({ "rows": [], "summary": { "tool_calls": 4, "tool_errors": 0, "files_changed": 1, "tokens_in": 900, "tokens_out": 100, "questions": { "asked": 0 } }, "finished": true }));
        }
        Reply::Json(404, json!({ "error": "no", "code": "not_found" }))
    });
    let ran = go(&stub.url(), &data, &["tui", "do it"], |_| {});
    assert_eq!(ran.code, 0, "{}", ran.err);
    assert!(ran.out.contains("  4 tool calls, 1 files changed, tokens 900 in / 100 out"), "{}", ran.out);
}

#[test]
fn with_the_api_token_the_view_shows_that_a_run_waits_and_never_asks_to_answer_it() {
    let data = data_dir("tui_waits");
    let frames = vec![
        row(1, started()),
        row(2, json!({ "type": "tool.started", "call_id": 1, "tool": "fs.edit" })),
        row(3, json!({ "type": "status", "state": "waiting_approval" })),
        row(4, json!({ "type": "approval.opened", "request": {} })),
        row(5, json!({ "type": "approval.closed", "request_id": "c_1", "outcome": "timeout" })),
        row(6, json!({ "type": "tool.finished", "call_id": 1, "ok": false, "status": "denied", "ms": 1, "decision": "denied", "approval": "timeout" })),
        row(7, finished("completed", "final")),
        end_frame(7),
    ];
    let stub = stub_with_stream(frames);
    let ran = go(&stub.url(), &data, &["tui", "edit it"], |_| {});
    assert_eq!(ran.code, 2, "a question nobody answered is the person's: {}", ran.err);
    assert!(ran.out.contains("! approval needed: the run waits for a yes or a no. Answer it in the Aqua app"), "{}", ran.out);
    assert!(ran.err.contains("the run needs you"), "{}", ran.err);
    for r in stub.requests() {
        assert_eq!(r.auth, format!("bearer {}", TOKEN.to_ascii_lowercase()), "only the API token is sent: {} {}", r.method, r.path);
        assert!(!r.path.contains("confirm") && !r.path.contains("answer") && !r.path.contains("approv"), "the view asked for {}", r.path);
    }
    assert!(!ran.out.contains(TOKEN) && !ran.err.contains(TOKEN) && !ran.out.contains(UI_TOKEN) && !ran.err.contains(UI_TOKEN));
}

#[test]
fn ui_reads_the_stream_with_the_apps_token_for_a_person_at_a_terminal_and_says_so_and_changes_nothing() {
    let data = data_dir("tui_ui");
    let frames = vec![
        row(1, started()),
        row(2, json!({ "type": "tool.started", "call_id": 3, "tool": "fs.edit", "args_head": "{\"path\":\"E:/work/a.txt\"}" })),
        row(3, json!({ "type": "approval.opened", "request": { "request_id": "c_1", "tool": "fs.edit", "reason": "Change a file", "preview": PREVIEW, "expires_in_secs": 90 } })),
        row(4, json!({ "type": "approval.closed", "request_id": "c_1", "outcome": "approved" })),
        row(5, json!({ "type": "tool.finished", "call_id": 3, "ok": true, "status": "completed", "ms": 5, "decision": "asked", "approval": "approved" })),
        row(6, finished("completed", "final")),
        end_frame(6),
    ];
    let stub = stub_with_stream(frames);
    // A script (no terminal) is refused before anything is read or sent.
    let script = go(&stub.url(), &data, &["tui", "edit it", "--ui"], |_| {});
    assert_eq!(script.code, 1);
    assert!(script.err.contains("needs a person at a terminal") && stub.requests().is_empty(), "{}", script.err);
    // A person at a terminal: the run is started with the API token and read with the app's, and the diff is on the screen.
    let ran = go(&stub.url(), &data, &["tui", "edit it", "--ui"], |io| io.stdin_is_terminal = true);
    assert_eq!(ran.code, 0, "{}", ran.err);
    assert!(ran.err.contains("--ui: this view reads with the app's own token (read only)"), "{}", ran.err);
    assert!(ran.out.contains("    -four") && ran.out.contains("    +FOUR") && ran.out.contains("! approval needed: Change a file (fs.edit)") && ran.out.contains("answer it in the Aqua app: this terminal cannot approve anything"), "{}", ran.out);
    let reqs = stub.requests();
    for r in &reqs {
        let expected = if r.path.contains("/stream") { UI_TOKEN } else { TOKEN };
        assert_eq!(r.auth, format!("bearer {}", expected.to_ascii_lowercase()), "{} {}", r.method, r.path);
        assert!(r.method == "GET" || r.path == "/api/v1/runs", "the app's token is for reading only: {} {}", r.method, r.path);
        assert!(!r.path.contains("confirm") && !r.path.contains("answer") && !r.path.contains("approv") && !r.path.contains("plan"));
    }
    assert!(!ran.out.contains(UI_TOKEN) && !ran.err.contains(UI_TOKEN) && !ran.out.contains(TOKEN) && !ran.err.contains(TOKEN));
    // The app's token is the data folder's, never a file the caller names.
    let api_token_file = data.join(".aqua_api_token");
    let named = go(&stub.url(), &data, &["tui", "edit it", "--ui", "--token-file", &api_token_file.to_string_lossy()], |io| io.stdin_is_terminal = true);
    assert_eq!(named.code, 1);
    assert!(named.err.contains("data folder only"), "{}", named.err);
}

#[test]
fn a_bad_command_line_is_refused_before_the_core_is_asked() {
    let data = data_dir("tui_bad_args");
    let stub = Stub::start(|_, _| Reply::Json(200, json!({})));
    for args in [
        vec!["tui", "x", "--format", "json"],
        vec!["tui", "x", "--nope"],
        vec!["tui", "x", "--width", "wide"],
        vec!["tui", "x", "--max-steps", "many"],
        vec!["tui", "x", "--session", "a b"],
        vec!["tui", "x", "--preapproval", "pa_1"],
        vec!["tui", "x", "--attach", "run_1"],
        vec!["tui", "--attach", "../etc"],
        // No goal and nobody who can type: it says how to use it.
        vec!["tui"],
    ] {
        let ran = go(&stub.url(), &data, &args, |_| {});
        assert_eq!(ran.code, 1, "{args:?}: {}", ran.err);
        assert!(!ran.err.is_empty(), "{args:?} says why");
    }
    assert!(stub.requests().is_empty(), "{:?}", stub.paths());
}

#[test]
fn attach_follows_a_run_that_exists_from_its_first_event_without_starting_one() {
    let data = data_dir("tui_attach");
    let stub = stub_with_stream(vec![row(1, started()), row(2, json!({ "type": "text", "index": 1, "text": "Earlier answer." })), row(3, finished("completed", "final")), end_frame(3)]);
    let ran = go(&stub.url(), &data, &["tui", "--attach", "run_stub_1"], |_| {});
    assert_eq!(ran.code, 0, "{}", ran.err);
    assert!(ran.out.contains("Earlier answer."));
    assert!(!stub.paths().iter().any(|p| p == "POST /api/v1/runs"), "{:?}", stub.paths());
    assert_eq!(stub.paths()[0], "GET /api/v1/runs/run_stub_1/stream?after=0");
}

#[test]
fn a_stream_that_breaks_is_taken_up_again_and_nothing_is_drawn_twice() {
    let data = data_dir("tui_reconnect");
    let stub = Stub::start(move |req, _| {
        if req.method == "POST" && req.path == "/api/v1/runs" {
            return start_answer();
        }
        if let Some(query) = req.path.strip_prefix("/api/v1/runs/run_stub_1/stream") {
            let first = query == "?after=0";
            return Reply::Stream(Box::new(move |c| {
                if first {
                    chunk(c, &row(1, started()));
                    chunk(c, &row(2, json!({ "type": "text", "index": 1, "text": "one" })));
                } else {
                    chunk(c, &row(2, json!({ "type": "text", "index": 1, "text": "one" })));
                    chunk(c, &row(3, json!({ "type": "text", "index": 2, "text": "two" })));
                    chunk(c, &row(4, finished("completed", "final")));
                    chunk(c, &end_frame(4));
                }
            }));
        }
        Reply::Json(404, json!({ "error": "no", "code": "not_found" }))
    });
    let ran = go(&stub.url(), &data, &["tui", "do it"], |_| {});
    assert_eq!(ran.code, 0, "{}\n{}", ran.out, ran.err);
    assert_eq!((ran.out.matches("one").count(), ran.out.matches("two").count()), (1, 1), "{}", ran.out);
    assert!(ran.err.contains("taking it up again"), "{}", ran.err);
}

#[test]
fn ctrl_c_ends_the_run_and_the_view_goes_on_to_say_how_it_ended() {
    let data = data_dir("tui_ctrl_c");
    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = interrupted.clone();
    let stub = Stub::start(move |req, _| {
        if req.method == "POST" && req.path == "/api/v1/runs" {
            return start_answer();
        }
        if req.path.ends_with("/interrupt") {
            flag.store(true, Ordering::SeqCst);
            return Reply::Json(200, json!({ "run_id": "run_stub_1", "interrupted": true, "ended": false }));
        }
        if req.path.starts_with("/api/v1/runs/run_stub_1/stream") {
            let flag = flag.clone();
            return Reply::Stream(Box::new(move |c| {
                chunk(c, &row(1, started()));
                let until = std::time::Instant::now() + Duration::from_secs(20);
                while !flag.load(Ordering::SeqCst) && std::time::Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(10));
                }
                chunk(c, &row(2, finished("cancelled", "cancelled")));
                chunk(c, &end_frame(2));
            }));
        }
        Reply::Json(404, json!({ "error": "no", "code": "not_found" }))
    });
    let interrupts = Arc::new(AtomicUsize::new(3));
    let pressed = interrupts.clone();
    // Three presses came before this run (they belong to the runs before it): this run's own press is the fourth.
    let watcher = {
        let seen = stub.seen.clone();
        std::thread::spawn(move || {
            let until = std::time::Instant::now() + Duration::from_secs(10);
            while !seen.lock().unwrap().iter().any(|r| r.path.contains("/stream")) && std::time::Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            std::thread::sleep(Duration::from_millis(100));
            pressed.fetch_add(1, Ordering::SeqCst);
        })
    };
    let ran = run_with(&stub.url(), &data, &["tui", "do it"], None, &interrupts, |_| {});
    watcher.join().unwrap();
    assert_eq!(ran.code, 4, "{}\n{}", ran.out, ran.err);
    assert!(interrupted.load(Ordering::SeqCst) && stub.paths().contains(&"POST /api/v1/runs/run_stub_1/interrupt".to_string()), "{:?}", stub.paths());
    assert!(ran.err.contains("stopping..."), "{}", ran.err);
    assert!(ran.out.contains("= cancelled"), "the view drew the end: {}", ran.out);
}

#[test]
fn a_line_typed_while_a_run_goes_is_said_to_it_and_queue_and_stop_are_words_of_the_view() {
    let data = data_dir("tui_typed");
    let told = Arc::new(Mutex::new(Vec::<String>::new()));
    let stopped = Arc::new(AtomicBool::new(false));
    let (told2, stopped2) = (told.clone(), stopped.clone());
    let stub = Stub::start(move |req, _| match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/api/v1/runs") => start_answer(),
        ("POST", "/api/v1/runs/run_stub_1/messages") => {
            told2.lock().unwrap().push(req.body.clone());
            Reply::Json(202, json!({ "run_id": "run_stub_1", "delivery": "steer", "queued": 1, "note": "it is read at the next step" }))
        }
        ("POST", "/api/v1/runs/run_stub_1/interrupt") => {
            stopped2.store(true, Ordering::SeqCst);
            Reply::Json(200, json!({ "interrupted": true, "ended": false }))
        }
        (_, p) if p.starts_with("/api/v1/runs/run_stub_1/stream") => {
            let stopped = stopped2.clone();
            Reply::Stream(Box::new(move |c| {
                chunk(c, &row(1, started()));
                let until = std::time::Instant::now() + Duration::from_secs(20);
                while !stopped.load(Ordering::SeqCst) && std::time::Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(10));
                }
                chunk(c, &row(2, finished("cancelled", "cancelled")));
                chunk(c, &end_frame(2));
            }))
        }
        _ => Reply::Json(404, json!({ "error": "no", "code": "not_found" })),
    });
    let (tx, rx) = channel::<String>();
    for line in ["also add a test", "/queue then run clippy", "/steer use strict mode", "/frobnicate", "/status", "/stop"] {
        tx.send(line.to_string()).unwrap();
    }
    let interrupts = AtomicUsize::new(0);
    let ran = run_with(&stub.url(), &data, &["tui", "do it"], Some(&rx), &interrupts, |_| {});
    assert_eq!(ran.code, 4, "{}\n{}", ran.out, ran.err);
    let bodies: Vec<Value> = told.lock().unwrap().iter().map(|b| serde_json::from_str(b).unwrap()).collect();
    assert_eq!(
        bodies.iter().map(|b| (b["text"].as_str().unwrap().to_string(), b["delivery"].as_str().unwrap().to_string())).collect::<Vec<_>>(),
        vec![("also add a test".to_string(), "steer".to_string()), ("then run clippy".to_string(), "queue".to_string()), ("use strict mode".to_string(), "steer".to_string())]
    );
    assert!(ran.out.contains("-> it is read at the next step"), "{}", ran.out);
    assert!(ran.err.contains("/frobnicate is not a word while a run goes"), "{}", ran.err);
    assert!(ran.out.contains("[running | 0:00]") || ran.out.contains("[starting | 0:00]"), "/status says it: {}", ran.out);
    assert!(stopped.load(Ordering::SeqCst), "/stop ended the run");
}

#[test]
fn at_a_terminal_with_no_goal_it_asks_for_goals_one_after_another_in_one_session() {
    let data = data_dir("tui_prompt");
    let stub = Stub::start(|req, _| match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/api/v1/runs") => start_answer(),
        ("GET", "/api/v1/sessions?limit=10") => Reply::Json(200, json!({ "sessions": [{ "id": "tui_1", "title": "Fix the parser", "runs": 2, "going": false }], "next": null })),
        ("GET", "/api/v1/runs/active") => Reply::Json(200, json!({ "runs": [] })),
        (_, p) if p.starts_with("/api/v1/runs/run_stub_1/stream") => Reply::Stream(Box::new(|c| {
            chunk(c, &row(1, started()));
            chunk(c, &row(2, finished("completed", "final")));
            chunk(c, &end_frame(2));
        })),
        _ => Reply::Json(404, json!({ "error": "no", "code": "not_found" })),
    });
    let (tx, rx) = channel::<String>();
    let paths_of = |stub: &Stub| stub.paths();
    let seen = stub.seen.clone();
    // The lines are typed one after another, each when the run before it is over (its transcript was asked for): a line typed during a run is said to the run.
    let typist = std::thread::spawn(move || {
        let ended = |n: usize| {
            let until = std::time::Instant::now() + Duration::from_secs(10);
            while seen.lock().unwrap().iter().filter(|r| r.path.ends_with("/transcript")).count() < n && std::time::Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        tx.send("fix the parser".into()).unwrap();
        ended(1);
        for line in ["/sessions", "/runs", "/new", "/help", "add a test"] {
            tx.send(line.into()).unwrap();
        }
        ended(2);
        tx.send("/quit".into()).unwrap();
    });
    let interrupts = AtomicUsize::new(0);
    let ran = run_with(&stub.url(), &data, &["tui"], Some(&rx), &interrupts, |io| io.stdin_is_terminal = true);
    typist.join().unwrap();
    assert_eq!(ran.code, 0, "{}\n{}", ran.out, ran.err);
    let starts: Vec<Value> = stub.requests().iter().filter(|r| r.method == "POST" && r.path == "/api/v1/runs").map(|r| serde_json::from_str(&r.body).unwrap()).collect();
    assert_eq!(starts.len(), 2, "{:?}", paths_of(&stub));
    assert_eq!((starts[0]["goal"].as_str(), starts[1]["goal"].as_str()), (Some("fix the parser"), Some("add a test")));
    let (first, second) = (starts[0]["session_id"].as_str().unwrap(), starts[1]["session_id"].as_str().unwrap());
    assert!(first.starts_with("tui_") && first != second, "/new made another session: {first} {second}");
    assert!(ran.out.contains("tui_1  Fix the parser (2 runs)") && ran.out.contains("no run of this token is going") && ran.out.contains("a new session") && ran.out.contains("/quit         leave"), "{}", ran.out);
    assert!(ran.out.contains("aqua> "), "the prompt is shown to a person at a terminal: {}", ran.out);
    assert_eq!(ran.out.matches("= completed").count(), 2);
}

#[test]
fn the_old_placeholder_is_not_what_tui_runs_any_more_and_the_new_view_needs_no_raw_mode() {
    // Every `tui` runs this view (the old two-box placeholder screen was deleted, card OA13-04); the view itself writes lines only.
    let source = include_str!("../view.rs");
    for needle in ["enable_raw_mode", "EnterAlternateScreen", "use crossterm", "use ratatui"] {
        assert!(!source.contains(needle), "the view uses no {needle}");
    }
}

#[test]
fn the_help_lists_the_view_and_the_editor_protocol() {
    let help = HELP.join("\n");
    assert!(help.contains("aqua-cli tui ") && help.contains("aqua-cli acp "));
    assert!(COMMANDS.contains(&"tui") && COMMANDS.contains(&"acp"));
}

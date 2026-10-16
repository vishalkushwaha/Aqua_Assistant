//! Coding by voice (card OA10-06; tasks UX-16 and UX-08 of the OpenCode audit): hearing what a run changed, going through it, and putting an undo on the screen.
//!
//! What a person who is coding with their hands busy needs from a voice, and what this module does about it:
//!
//! * **Switches.** Three, all off ([`code_speech_on`] `AQUA_CODE_SPEECH`, [`project_terms_on`] `AQUA_PROJECT_TERMS`, [`enabled`] `AQUA_CODING_VOICE`); this module is the one place the pipeline,
//!   the dictation route and the API read them, so that those files name no flag of their own.
//! * **Reading back** ([`handle_utterance`]): "read the diff", "next change", "previous change", "line by line", "repeat" are answered by the program with no model, from the diff of the last
//!   run that changed files (`readback`). The patch is masked when it is read here, before it is made into words (a key that was split into words is no longer a key the masker knows), and
//!   what is said is masked once more.
//! * **Staging** ([`StagedStore`]): "show me the diff" and "undo the last run" put a card in a short list that the app shows (`GET /api/v1/coding/staged`). **A voice cannot undo anything:** the
//!   card holds the run and a sentence; the app asks the dry run of `POST /api/v1/runs/{id}/undo` and the person presses the button there, with the UI token, which a voice does not have. This
//!   file has no call to a tool, no undo and no word of approval in it, and a test reads it to keep it so.
//!
//! The grammar of the phrases is `voice_commands::coding_command`: the whole sentence must be one of them, so that "read the diff of the movie reviews" is still a request.

use crate::readback::{self, FileChange, Item};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Where a run's diff is read from: the body of `GET /api/v1/runs/{id}/diff`, whose patches are masked. The gateway sets it when it starts (`coding_api::install`), so that this module names no route.
static DIFF_READER: std::sync::OnceLock<fn(&str) -> Value> = std::sync::OnceLock::new();

/// Says where a run's diff is read from. Called once, when the gateway starts; the first call wins.
pub fn install_diff_reader(read: fn(&str) -> Value) {
    let _ = DIFF_READER.set(read);
}

/// The diff of a run, or nothing (`null`) when no reader was installed.
fn read_diff(run_id: &str) -> Value {
    DIFF_READER.get().map_or(Value::Null, |read| read(run_id))
}

/// What was read stays the one to go on from for this long; after it "next change" says to read the diff again.
pub const READER_STALE_MS: i64 = 15 * 60 * 1000;
/// A card waits for the person this long, and at most [`MAX_STAGED`] wait.
pub const STAGED_TTL_MS: i64 = 10 * 60 * 1000;
pub const MAX_STAGED: usize = 5;

/// Is code left out of what is spoken (`AQUA_CODE_SPEECH`)? Off: a fenced block is read aloud as it always was.
pub fn code_speech_on() -> bool {
    crate::flags::is_on(&crate::flags::CODE_SPEECH)
}

/// Are the words of the project given to the speech model of a dictation clip (`AQUA_PROJECT_TERMS`)?
pub fn project_terms_on() -> bool {
    crate::flags::is_on(&crate::flags::PROJECT_TERMS)
}

/// Are the phrases of [`handle_utterance`] answered (`AQUA_CODING_VOICE`)?
pub fn enabled() -> bool {
    crate::flags::is_on(&crate::flags::CODING_VOICE)
}

/// What is spoken, masked: it is made from the text of files, and anybody in the room hears it. The masker is the one the run log and the tool results go through.
pub fn mask_spoken(text: &str) -> String {
    aqua_tool_host::output::Redactor::new().redact(text)
}

/// Where the program finds the runs and what they changed. The real ones read the run record and the diff route; a test gives its own.
pub struct Sources<'a> {
    /// The runs that ended, newest first, each with how many files it changed (from its `run.finished` event).
    pub finished: &'a dyn Fn() -> Vec<(String, u64)>,
    /// What a run changed: the body of `GET /api/v1/runs/{id}/diff`, whose patches are masked already.
    pub diff_of: &'a dyn Fn(&str) -> Value,
}

/// What was read last, so that "next change" has a next. One person, one reader.
#[derive(Debug, Default)]
pub struct Reader {
    run_id: String,
    items: Vec<Item>,
    /// The item that was read last; `None` before the first.
    at: Option<usize>,
    last: String,
    touched_ms: i64,
}

impl Reader {
    fn fresh(&self, now_ms: i64) -> bool {
        !self.items.is_empty() && now_ms - self.touched_ms <= READER_STALE_MS
    }

    fn say(&mut self, text: String, now_ms: i64) -> String {
        self.last = text.clone();
        self.touched_ms = now_ms;
        text
    }
}

/// A card for the app. It holds what to show and for which run; it can do nothing by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedItem {
    pub id: String,
    /// `undo_run` or `show_diff`.
    pub kind: &'static str,
    pub run_id: String,
    pub text: String,
    pub created_ms: i64,
    pub expires_ms: i64,
}

impl StagedItem {
    fn view(&self) -> Value {
        let how = match self.kind {
            "undo_run" => format!("Ask POST /api/v1/runs/{}/undo with dry_run true to show what would be put back, and press the button to do it: that call needs the UI token, which a voice does not have.", self.run_id),
            _ => format!("Show GET /api/v1/runs/{}/diff.", self.run_id),
        };
        json!({ "id": self.id, "kind": self.kind, "run_id": self.run_id, "text": self.text, "created_ms": self.created_ms, "expires_ms": self.expires_ms, "how": how })
    }
}

/// The cards the voice put on the screen and the person has not answered. In memory only: a restart forgets them, which is the safe way to forget a pending undo.
pub struct StagedStore {
    items: Mutex<Vec<StagedItem>>,
    next: AtomicU64,
}

impl Default for StagedStore {
    fn default() -> Self {
        Self::new()
    }
}

impl StagedStore {
    pub const fn new() -> Self {
        Self { items: Mutex::new(Vec::new()), next: AtomicU64::new(1) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<StagedItem>> {
        self.items.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Puts a card in the list: the same kind for the same run replaces the old one, a card that has waited too long is gone, and when more than [`MAX_STAGED`] wait the oldest goes.
    pub fn stage(&self, kind: &'static str, run_id: &str, text: &str, now_ms: i64) -> StagedItem {
        let item = StagedItem {
            id: format!("st_{}", self.next.fetch_add(1, Ordering::Relaxed)),
            kind,
            run_id: run_id.to_string(),
            text: text.chars().take(300).collect(),
            created_ms: now_ms,
            expires_ms: now_ms + STAGED_TTL_MS,
        };
        let mut items = self.lock();
        items.retain(|i| i.expires_ms > now_ms && !(i.kind == kind && i.run_id == run_id));
        items.push(item.clone());
        while items.len() > MAX_STAGED {
            items.remove(0);
        }
        item
    }

    /// The cards that are still waiting, oldest first, as the route shows them.
    pub fn list(&self, now_ms: i64) -> Vec<Value> {
        let mut items = self.lock();
        items.retain(|i| i.expires_ms > now_ms);
        items.iter().map(StagedItem::view).collect()
    }

    /// The person put a card away. `false` when there is no such card (it may have expired).
    pub fn dismiss(&self, id: &str) -> bool {
        let mut items = self.lock();
        let before = items.len();
        items.retain(|i| i.id != id);
        items.len() != before
    }

    /// The cards for a run are done with (the undo was pressed, or the run was deleted).
    pub fn forget_run(&self, run_id: &str) -> usize {
        let mut items = self.lock();
        let before = items.len();
        items.retain(|i| i.run_id != run_id);
        before - items.len()
    }
}

/// The cards of the one person this core serves.
pub fn staged() -> &'static StagedStore {
    static STAGED: StagedStore = StagedStore::new();
    &STAGED
}

fn reader() -> &'static Mutex<Reader> {
    static READER: Mutex<Reader> = Mutex::new(Reader { run_id: String::new(), items: Vec::new(), at: None, last: String::new(), touched_ms: 0 });
    &READER
}

/// Milliseconds since the epoch.
fn now_ms() -> i64 {
    crate::agent_runs::now_ms()
}

/// How many files the run `id` changed, from its last event.
fn files_changed_of(store: &crate::agent_runs::AgentRunStore, id: &str) -> u64 {
    let last = store.last_event_seq(id).unwrap_or(0);
    if last == 0 {
        return 0;
    }
    store.events_after(id, last - 1, 2).ok().and_then(|rows| rows.into_iter().find(|r| r.kind == "run.finished")).and_then(|r| r.data.get("files_changed").and_then(Value::as_u64)).unwrap_or(0)
}

/// The runs that ended, newest first, with the number of files each changed.
fn finished_runs() -> Vec<(String, u64)> {
    let Some(store) = crate::agent_runs::store() else { return Vec::new() };
    let Ok(runs) = store.list(15, 0, None) else { return Vec::new() };
    runs.iter()
        .filter(|r| !matches!(r["status"].as_str(), Some("running" | "waiting_user" | "stalled" | "queued")))
        .filter_map(|r| r["id"].as_str())
        .map(|id| (id.to_string(), files_changed_of(store, id)))
        .collect()
}

/// The run that changed files last, and how many it changed.
fn last_changing_run(src: &Sources<'_>) -> Option<(String, u64)> {
    (src.finished)().into_iter().find(|(_, files)| *files > 0)
}

/// What the run changed, from the diff route's body, with every patch masked before it is made into words.
fn changes_of_run(src: &Sources<'_>, run_id: &str) -> Vec<FileChange> {
    let mut changes = readback::changes_of(&(src.diff_of)(run_id));
    for c in &mut changes {
        c.patch = mask_spoken(&c.patch);
    }
    changes
}

const NOTHING_TO_READ: &str = "No run that ended changed a file, as far as I know.";

/// "Change 2 of 5: main dot R S: replaced the call to parse with parse strict in function load, line 40."
fn say_item(items: &[Item], at: usize) -> String {
    format!("Change {} of {}: {}", at + 1, items.len(), items[at].sentence)
}

/// Makes the reader hold the changes of the last run that changed files. `Err` is what is said when there is nothing to read.
fn load(reader: &mut Reader, src: &Sources<'_>, now_ms: i64) -> Result<Vec<FileChange>, String> {
    let Some((run_id, files)) = last_changing_run(src) else { return Err(NOTHING_TO_READ.to_string()) };
    let changes = changes_of_run(src, &run_id);
    if changes.is_empty() {
        // The run says it changed files and no record of them was kept (the checkpoints are off): it cannot be read, and it can still be undone on the screen.
        return Err(format!("The last run changed {files} {}, but I did not keep what they said, so I cannot read them out. You can undo it from the screen.", if files == 1 { "file" } else { "files" }));
    }
    reader.run_id = run_id;
    reader.items = readback::items(&changes);
    reader.at = None;
    reader.touched_ms = now_ms;
    Ok(changes)
}

/// The reply to a coding phrase, or `None` when `text` is not one (or is "repeat" with nothing to repeat, which is a request like any other). The state is given, so that a test has its own.
pub fn handle_with(text: &str, wake: &str, now_ms: i64, reader: &mut Reader, staged: &StagedStore, src: &Sources<'_>) -> Option<String> {
    use crate::voice_commands::CodingCommand as C;
    let command = crate::voice_commands::coding_command(text, wake)?;
    let reply = match command {
        C::ReadDiff => match load(reader, src, now_ms) {
            Ok(changes) => {
                let summary = readback::summarise(&changes);
                let hint = if reader.items.len() > 1 { " Say next change to go through them." } else { "" };
                reader.say(format!("{summary}{hint}"), now_ms)
            }
            Err(said) => reader.say(said, now_ms),
        },
        C::NextChange | C::PreviousChange => {
            if !reader.fresh(now_ms) {
                match load(reader, src, now_ms) {
                    Ok(_) => {}
                    Err(said) => return Some(mask_spoken(&reader.say(said, now_ms))),
                }
            }
            let n = reader.items.len();
            let next = match (command, reader.at) {
                (C::NextChange, None) => Some(0),
                (C::NextChange, Some(a)) => (a + 1 < n).then_some(a + 1),
                (_, Some(a)) if a > 0 => Some(a - 1),
                _ => None,
            };
            match next {
                Some(i) => {
                    reader.at = Some(i);
                    let said = say_item(&reader.items, i);
                    reader.say(said, now_ms)
                }
                None if command == C::NextChange => reader.say("That was the last change.".into(), now_ms),
                None => reader.say("That is the first change.".into(), now_ms),
            }
        }
        C::LineByLine => {
            if !reader.fresh(now_ms) {
                match load(reader, src, now_ms) {
                    Ok(_) => {}
                    Err(said) => return Some(mask_spoken(&reader.say(said, now_ms))),
                }
            }
            let i = reader.at.unwrap_or(0);
            reader.at = Some(i);
            match reader.items[i].hunk.as_ref() {
                Some(hunk) => {
                    let said = format!("{} Line by line: {}", say_item(&reader.items, i), readback::spell_hunk(hunk));
                    reader.say(said, now_ms)
                }
                None => reader.say("There are no lines to spell for that one.".into(), now_ms),
            }
        }
        C::Repeat => {
            if !reader.fresh(now_ms) || reader.last.is_empty() {
                return None;
            }
            let said = reader.last.clone();
            reader.say(said, now_ms)
        }
        C::ShowDiff => match last_changing_run(src) {
            Some((run_id, files)) => {
                staged.stage("show_diff", &run_id, &format!("The changes of the last run: {files} {}", if files == 1 { "file" } else { "files" }), now_ms);
                reader.say("I put the changes on your screen.".into(), now_ms)
            }
            None => reader.say(NOTHING_TO_READ.to_string(), now_ms),
        },
        C::UndoLastRun => match last_changing_run(src) {
            Some((run_id, files)) => {
                let what = format!("{files} {}", if files == 1 { "file" } else { "files" });
                staged.stage("undo_run", &run_id, &format!("Undo the last run: put back {what}"), now_ms);
                reader.say(format!("I put the undo on your screen: {what} would be put back. Nothing is undone until you press it there."), now_ms)
            }
            None => reader.say("I have nothing to undo: no run that ended changed a file, as far as I know.".to_string(), now_ms),
        },
    };
    Some(mask_spoken(&reply))
}

/// The reply to a coding phrase said to Aqua, or `None` when it is not one or the setting is off (`AQUA_CODING_VOICE`). The program answers; no model is asked and nothing is changed.
pub fn handle_utterance(text: &str) -> Option<String> {
    if !enabled() {
        return None;
    }
    // Cheap first: only the phrases of the grammar get past here.
    crate::voice_commands::coding_command(text, "Hey Aqua")?;
    let finished = finished_runs;
    let src = Sources { finished: &finished, diff_of: &read_diff };
    let mut reader = reader().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    handle_with(text, "Hey Aqua", now_ms(), &mut reader, staged(), &src)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH_A: &str = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -38,5 +38,5 @@ fn load(path: &str) -> Config {\n     let text = read(path);\n     let n = 1;\n-    let cfg = parse(&text);\n+    let cfg = parse_strict(&text);\n     check(&cfg);\n     cfg\n@@ -90,2 +90,4 @@ fn save(cfg: &Config) {\n     write(cfg);\n+    log(\"saved\");\n+    flush();";
    const PATCH_B: &str = "--- a/lib/util.py\n+++ b/lib/util.py\n@@ -1,2 +1,2 @@\n import os\n-print(1)\n+print(2)";

    fn diff_body() -> Value {
        json!({
            "run_id": "run_new",
            "checkpoint": { "files": [
                { "path": "src/main.rs", "status": "M", "patch": PATCH_A, "truncated": false, "binary": false },
                { "path": "lib/util.py", "status": "M", "patch": PATCH_B, "truncated": false, "binary": false },
            ], "paths": [], "changed": 2, "more_files": 0 },
            "commands": []
        })
    }

    struct World {
        reader: Reader,
        staged: StagedStore,
        runs: Vec<(String, u64)>,
        body: Value,
        diff_calls: std::cell::Cell<usize>,
    }

    impl World {
        fn new() -> Self {
            World { reader: Reader::default(), staged: StagedStore::new(), runs: vec![("run_new".into(), 2), ("run_old".into(), 1)], body: diff_body(), diff_calls: std::cell::Cell::new(0) }
        }

        fn say(&mut self, text: &str, now_ms: i64) -> Option<String> {
            let runs = self.runs.clone();
            let finished = move || runs.clone();
            let body = self.body.clone();
            let calls = &self.diff_calls;
            let diff_of = move |_: &str| {
                calls.set(calls.get() + 1);
                body.clone()
            };
            let src = Sources { finished: &finished, diff_of: &diff_of };
            handle_with(text, "Hey Aqua", now_ms, &mut self.reader, &self.staged, &src)
        }
    }

    #[test]
    fn the_diff_is_read_as_a_summary_and_then_one_change_at_a_time() {
        let mut w = World::new();
        let summary = w.say("read the diff", 1_000).unwrap();
        assert_eq!(summary, "2 files changed, 4 lines added and 2 removed: main dot R S and util dot P Y. Say next change to go through them.");
        let first = w.say("next change", 2_000).unwrap();
        assert_eq!(first, "Change 1 of 3: main dot R S: replaced the call to parse with parse strict in function load, line 40.");
        let second = w.say("Hey Aqua, next change", 3_000).unwrap();
        assert!(second.starts_with("Change 2 of 3: main dot R S: added 2 lines in function save, lines 91 to 92."), "{second}");
        assert_eq!(w.say("repeat that", 4_000).as_deref(), Some(second.as_str()));
        let third = w.say("next change", 5_000).unwrap();
        assert!(third.starts_with("Change 3 of 3: util dot P Y: replaced 1 with 2"), "{third}");
        assert_eq!(w.say("next change", 6_000).as_deref(), Some("That was the last change."));
        let back = w.say("previous change", 7_000).unwrap();
        assert!(back.starts_with("Change 2 of 3:"), "{back}");
        assert!(w.say("previous change", 8_000).unwrap().starts_with("Change 1 of 3:"));
        assert_eq!(w.say("previous change", 9_000).as_deref(), Some("That is the first change."));
        assert_eq!(w.diff_calls.get(), 1, "the run is read once, and the next changes come from what was read");
    }

    #[test]
    fn line_by_line_spells_the_change_that_was_read_last() {
        let mut w = World::new();
        w.say("read the diff", 1_000);
        w.say("next change", 2_000);
        let spelled = w.say("line by line", 3_000).unwrap();
        assert!(spelled.starts_with("Change 1 of 3: main dot R S: replaced the call to parse with parse strict in function load, line 40. Line by line: line 40 removed: let cfg equals parse open paren ampersand text close paren semicolon."), "{spelled}");
        assert!(spelled.contains("line 40 added: let cfg equals parse strict open paren"), "{spelled}");
        // With nothing read yet it reads the first change.
        let mut fresh = World::new();
        assert!(fresh.say("read it line by line", 1_000).unwrap().contains("Line by line: line 40 removed"));
    }

    #[test]
    fn what_is_said_is_masked_and_a_key_in_a_patch_is_not_spelled_out() {
        let mut w = World::new();
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        w.body = json!({ "checkpoint": { "files": [{ "path": "src/keys.rs", "status": "M", "patch": format!("--- a/k\n+++ b/k\n@@ -1 +1 @@\n-let k = 1;\n+let k = \"{secret}\";"), "truncated": false, "binary": false }] }, "commands": [] });
        w.say("read the diff", 1_000);
        let spelled = w.say("line by line", 2_000).unwrap();
        assert!(!spelled.contains("abcdefghijklmnopqrstuvwxyz") && !spelled.contains("ghp") && spelled.contains("REDACTED"), "{spelled}");
    }

    #[test]
    fn the_undo_is_put_on_the_screen_and_the_voice_does_nothing_more() {
        let mut w = World::new();
        let said = w.say("undo the last run", 1_000).unwrap();
        assert_eq!(said, "I put the undo on your screen: 2 files would be put back. Nothing is undone until you press it there.");
        let cards = w.staged.list(1_001);
        assert_eq!(cards.len(), 1);
        assert_eq!((cards[0]["kind"].as_str(), cards[0]["run_id"].as_str()), (Some("undo_run"), Some("run_new")));
        assert!(cards[0]["how"].as_str().unwrap().contains("UI token") && cards[0]["how"].as_str().unwrap().contains("/api/v1/runs/run_new/undo"), "{}", cards[0]);
        // Said twice: one card. "Show me the diff" is another kind of card.
        w.say("undo the last run", 2_000);
        w.say("show me the diff", 3_000);
        let kinds: Vec<_> = w.staged.list(3_001).iter().map(|c| c["kind"].as_str().unwrap().to_string()).collect();
        assert_eq!(kinds, ["undo_run", "show_diff"]);
        // Nothing to undo: nothing is staged.
        let mut empty = World::new();
        empty.runs = vec![("run_a".into(), 0)];
        assert_eq!(empty.say("undo the last run", 1_000).as_deref(), Some("I have nothing to undo: no run that ended changed a file, as far as I know."));
        assert!(empty.staged.list(1_001).is_empty());
        // An approval is never taken for one of the phrases, and nothing is staged by it.
        for yes in ["yes", "approve", "go ahead", "do it", "yes undo the last run", "approve the undo"] {
            assert_eq!(w.say(yes, 4_000), None, "{yes}");
        }
        assert_eq!(w.staged.list(4_001).len(), 2);
    }

    /// The voice cannot undo, approve or call a tool: this file holds no call that could. (The words are looked for in the part of the file that ships, not in the tests.)
    #[test]
    fn the_file_has_no_call_to_a_tool_no_undo_and_no_approval() {
        let source = include_str!("coding_voice.rs");
        let shipping = source.split("#[cfg(test)]").next().unwrap();
        for forbidden in ["call_blocking", "ToolRequest", "ToolHost", "RequestContext", "Caller::", ".undo(", "fs.undo_run", "answer_question", "confirm(", "approve("] {
            assert!(!shipping.contains(forbidden), "coding_voice must not contain {forbidden}");
        }
    }

    #[test]
    fn a_run_with_no_record_of_its_changes_says_so_and_can_still_be_undone() {
        let mut w = World::new();
        w.body = json!({ "checkpoint": null, "commands": [] });
        let said = w.say("read the diff", 1_000).unwrap();
        assert_eq!(said, "The last run changed 2 files, but I did not keep what they said, so I cannot read them out. You can undo it from the screen.");
        assert!(w.say("undo the last run", 2_000).unwrap().contains("2 files would be put back"));
        let mut none = World::new();
        none.runs = Vec::new();
        assert_eq!(none.say("read the diff", 1_000).as_deref(), Some(NOTHING_TO_READ));
        assert_eq!(none.say("next change", 2_000).as_deref(), Some(NOTHING_TO_READ));
    }

    #[test]
    fn repeat_with_nothing_read_is_a_request_like_any_other_and_a_stale_reader_is_read_again() {
        let mut w = World::new();
        assert_eq!(w.say("repeat that", 1_000), None, "nothing was read: the model may answer it");
        assert_eq!(w.say("what is the weather", 1_000), None);
        w.say("read the diff", 2_000);
        w.say("next change", 3_000);
        assert!(w.say("repeat", 4_000).is_some());
        // After fifteen minutes of quiet the next change is the first of the run read again (the last thing said was at 4,000).
        let later = 4_000 + READER_STALE_MS + 1;
        let first = w.say("next change", later).unwrap();
        assert!(first.starts_with("Change 1 of 3:"), "{first}");
        assert_eq!(w.diff_calls.get(), 2);
        assert_eq!(w.say("repeat", later + READER_STALE_MS + 5), None, "a stale reading is not repeated");
    }

    #[test]
    fn the_cards_are_few_short_lived_and_can_be_put_away() {
        let store = StagedStore::new();
        for i in 0..7 {
            store.stage("undo_run", &format!("run_{i}"), "text", 1_000 + i);
        }
        let ids: Vec<String> = store.list(2_000).iter().map(|c| c["run_id"].as_str().unwrap().to_string()).collect();
        assert_eq!(ids, ["run_2", "run_3", "run_4", "run_5", "run_6"], "the oldest go");
        assert!(store.list(2_000 + STAGED_TTL_MS).is_empty(), "a card that has waited too long is gone");
        let card = store.stage("show_diff", "run_9", &"x".repeat(1_000), 5_000);
        assert_eq!(card.text.chars().count(), 300);
        assert!(store.dismiss(&card.id) && !store.dismiss(&card.id));
        store.stage("undo_run", "run_9", "t", 6_000);
        store.stage("show_diff", "run_9", "t", 6_000);
        assert_eq!(store.forget_run("run_9"), 2);
        assert!(store.list(6_001).is_empty());
    }

    #[test]
    fn the_switches_are_off_until_they_are_set() {
        let all_off = [("AQUA_CODE_SPEECH", None), ("AQUA_PROJECT_TERMS", None), ("AQUA_CODING_VOICE", None)];
        crate::flags::with_overrides(&all_off, || {
            assert!(!code_speech_on() && !project_terms_on() && !enabled());
            assert_eq!(handle_utterance("undo the last run"), None, "off: the phrase is a request like any other");
        });
        crate::flags::with_overrides(&[("AQUA_CODE_SPEECH", Some("on")), ("AQUA_PROJECT_TERMS", Some("1")), ("AQUA_CODING_VOICE", Some("true"))], || {
            assert!(code_speech_on() && project_terms_on() && enabled());
        });
    }
}

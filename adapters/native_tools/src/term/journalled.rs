//! The terminal tools and the journal of what a command changed (card OA7-03, TM-12). Compiled with the `git` feature: the journal works with git's own object store.
//!
//! With the journal on (`journal` in `term.json`, or `AQUA_TERM_JOURNAL=on`) a `term.run` or `term.shell` that may change files, started in a repository inside the folders Aqua may
//! use, is looked at before and after (see `git/journal.rs`), the list of files it changed ends its result, and `term.undo` puts them back. With it off (the default) nothing here
//! runs: the tools are as they were and `term.undo` is not offered.

use super::*;
use crate::git::journal::{label_of, may_change_files, restore_lines, Begun, Journal, Journalled, KEEP_DAYS};
use std::collections::HashSet;

/// The most files a question about `term.undo` looks at to say whether one of them is a secret or a program.
const CLASSIFY_FILES: usize = 200;

impl TermProvider {
    /// Gives the terminal tools the journal of commands. Called once, where the tools are put together, before the first call.
    pub fn set_journal(&self, journal: Arc<Journal>) -> bool {
        self.0.journal.set(journal).is_ok()
    }
}

impl TermInner {
    /// Whether commands are looked at and `term.undo` is on offer: `journal` of term.json, or `AQUA_TERM_JOURNAL=on`.
    pub(super) fn journal_on(&self) -> bool {
        self.cfg.journal || matches!(std::env::var("AQUA_TERM_JOURNAL").ok().as_deref().map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("on" | "1" | "true"))
    }

    fn journal(&self) -> Option<&Arc<Journal>> {
        if self.journal_on() {
            self.journal.get()
        } else {
            None
        }
    }

    /// The folder a call runs in: the one it names, else the first root. `None` when it cannot be told (the call itself says why).
    fn folder_of(&self, args: &Value) -> Option<PathBuf> {
        match args.get("cwd").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            Some(c) => self.guard.resolve(c, Op::Read).ok().map(|r| r.path),
            None => self.guard.roots().first().map(|r| r.1.clone()),
        }
    }

    /// Runs `work` (a `term.run` or a `term.shell`) between two looks at the repository it runs in, when the journal is on and the command may change files. What it changed ends the
    /// text of the result (or of the error: a command that timed out or was stopped has done what it did), and the data carries it too. A call the journal cannot look at runs as it did.
    pub(super) fn journalled(&self, tool: &str, args: &Value, run_id: &str, cancel: &Arc<AtomicBool>, work: impl FnOnce() -> Result<ProviderOutput, ToolError>) -> Result<ProviderOutput, ToolError> {
        let Some(journal) = self.journal().cloned() else { return work() };
        let given = |v: &Value| -> Vec<String> { v.get("args").and_then(|a| a.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default() };
        let (changes, label) = match tool {
            "term.run" => {
                let exe = args.get("exe").and_then(|v| v.as_str()).unwrap_or("");
                let argv = given(args);
                (may_change_files(exe, &argv), label_of(exe, &argv))
            }
            "term.shell" => {
                let segs = rules::shell_segments(args.get("command").and_then(|v| v.as_str()).unwrap_or("")).unwrap_or_default();
                let labels: Vec<String> = segs.iter().filter(|(e, a)| may_change_files(e, a)).map(|(e, a)| label_of(e, a)).collect();
                (!labels.is_empty(), labels.join(" && ").chars().take(80).collect())
            }
            _ => (false, String::new()),
        };
        // A command that runs isolated can write only its scratch folder; one that runs at a tier cannot write the repository either.
        let isolated = tool == "term.run" && args.get("isolated").and_then(|v| v.as_bool()).unwrap_or(false);
        if !changes || isolated || self.tier != Tier::None {
            return work();
        }
        let Some(cwd) = self.folder_of(args) else { return work() };
        let began = journal.begin(&self.guard, &cwd, cancel);
        let mut result = work();
        // What the look before said: nothing to look at (no repository: nothing is added to the result), or a look that was given up (the result says so), or the first tree.
        let (done, data) = match began {
            Begun::NotInARepository => return result,
            Begun::GaveUp(why) => {
                let done = Journalled { record: None, changes: Vec::new(), skipped_large: 0, millis: 0, not_kept: None, gave_up: Some(why) };
                let data = done.data();
                (done, data)
            }
            Begun::Before(before) => {
                // A command that was stopped has still done what it did: the second look is made whatever the flag of the call says (it is bounded to five seconds).
                let not_stopped = AtomicBool::new(false);
                let done = journal.finish(before, &self.guard, run_id, &label, &not_stopped);
                let data = done.data();
                (done, data)
            }
        };
        let text = done.text();
        match &mut result {
            Ok(out) => {
                if !text.is_empty() {
                    out.text.push_str(&text);
                    if out.data.get("promoted").and_then(|v| v.as_bool()).unwrap_or(false) {
                        out.text.push_str(" (The command is still running as a background job: what it changes later is not in this list.)");
                    }
                }
                if let Some(obj) = out.data.as_object_mut() {
                    obj.insert("journal".into(), data);
                }
            }
            Err(e) => {
                if !text.is_empty() {
                    e.message.push_str(&format!("\nbefore it ended: {}", text.trim_start()));
                }
            }
        }
        result
    }

    fn journal_for_undo(&self) -> Result<&Arc<Journal>, ToolError> {
        self.journal.get().ok_or_else(|| ToolError::new(ErrorCode::Unavailable, "term.undo is off: the journal of commands is not set up"))
    }

    /// The question for `term.undo`: always asked, in every mode, and it says how many files go back.
    pub(super) fn classify_undo(&self, args: &Value) -> Result<Classification, ToolError> {
        let journal = self.journal_for_undo()?;
        let id = args.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
        let cwd = self.folder_of(args);
        let nobody_stops_it = AtomicBool::new(false);
        let record = journal.find(&self.guard, id, cwd.as_deref(), &nobody_stops_it).ok_or_else(|| not_found(id))?;
        let mut classes = vec![PermClass::Write, PermClass::Destructive];
        // The files that go back are read through the file guard as any write is: a secret or a program among them says so in the question.
        let changed = journal.changes_of(&record, &nobody_stops_it)?;
        for c in changed.iter().take(CLASSIFY_FILES) {
            if let Ok(r) = self.guard.resolve(&record.root.join(&c.path).to_string_lossy(), Op::Write) {
                if r.sensitive && !classes.contains(&PermClass::Credential) {
                    classes.push(PermClass::Credential);
                }
                if r.executable_kind && !classes.contains(&PermClass::Execute) {
                    classes.push(PermClass::Execute);
                }
            }
        }
        let dry = args.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false);
        let what = if record.label.is_empty() { "a command".to_string() } else { format!("`{}`", record.label) };
        Ok(Classification {
            classes,
            target: display(&record.root),
            summary: format!("Undo {what} ({id}): put back the {} file{} it changed in {}; a file changed since is left{}", changed.len(), if changed.len() == 1 { "" } else { "s" }, display(&record.root), if dry { " (dry run: nothing is changed)" } else { "" }),
            // It asks every time, whatever the mode and whatever the folder: putting files back is not something a grant for the session covers.
            target_allowed: false,
        })
    }

    pub(super) fn do_undo(&self, args: &Value, cancel: &Arc<AtomicBool>) -> Result<ProviderOutput, ToolError> {
        let journal = self.journal_for_undo()?;
        let id = args.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
        let cwd = self.folder_of(args);
        let record = journal.find(&self.guard, id, cwd.as_deref(), cancel).ok_or_else(|| not_found(id))?;
        let dry = args.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut handled: HashSet<String> = HashSet::new();
        let done = journal.undo(&record, &self.guard, dry, &mut handled, cancel)?;
        if !dry && done.left.is_empty() {
            journal.settle(&record.id);
        }
        let mut text = format!(
            "{} {} ({}): {} put back, {} taken away, {} left{}.",
            if dry { "Dry run of the undo of" } else { "Undid" },
            if record.label.is_empty() { "the command".to_string() } else { format!("`{}`", record.label) },
            record.id,
            done.restored.len(),
            done.removed.len(),
            done.left.len(),
            if done.already.is_empty() { String::new() } else { format!(", {} already as they were", done.already.len()) },
        );
        for line in restore_lines(&done, "", dry) {
            text.push('\n');
            text.push_str(&line);
        }
        if !done.left.is_empty() {
            text.push_str("\nLeft as they are:");
            for (path, why) in &done.left {
                text.push_str(&format!("\n{path}: {why}"));
            }
        }
        out(
            json!({
                "call_id": record.id,
                "dry_run": dry,
                "restored": done.restored,
                "removed": done.removed,
                "already": done.already,
                "left": done.left.iter().map(|(p, w)| json!({"path": p, "why": w})).collect::<Vec<_>>(),
            }),
            text,
        )
    }
}

fn not_found(id: &str) -> ToolError {
    ToolError::new(ErrorCode::NotFound, format!("no journalled command has the call_id '{id}': it is named in the result of a command that changed files, and kept {KEEP_DAYS} days (after a restart of Aqua, say which repository with cwd)"))
}

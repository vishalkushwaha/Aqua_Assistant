//! Coding-session policy for a goal the code verifier already treats as coding.
//!
//! These rules apply inside a coding run. They do not turn the process-wide switches
//! (`AQUA_CONTEXT`, `AQUA_PLAN`, `AQUA_ASK`) on for every other goal. A coding run always
//! uses the local planner: the `AQUA_REMOTE_CODING*` switches are retired (card OA1-03)
//! and change nothing. This module never opens a connection.

use crate::modules::planner::plan;
use crate::modules::run_profile::{coding, coding_latched};

/// A coding goal uses the context manager even when `AQUA_CONTEXT` is unset.
pub fn use_context_v2(goal: &str) -> bool {
    crate::modules::context::v2_enabled() || coding(goal)
}

/// A coding goal of two steps or more may carry a plan even when `AQUA_PLAN` is unset.
pub fn wants_plan(goal: &str) -> bool {
    plan::goal_needs_plan(goal) && (plan::enabled() || coding(goal))
}

/// A coding goal may ask one question even when `AQUA_ASK` is unset.
pub fn allow_ask(goal: &str) -> bool {
    crate::modules::planner::schema::ask_enabled() || coding(goal)
}

/// Tools a coding run must be able to see when the host registered them.
pub const PINNED_TOOLS: &[&str] = &["fs.grep", "fs.patch", "fs.list", "git.status", "git.diff"];

/// Tools a coding run is also pinned when they are on offer at all: `fs.edit_lines` exists only with `AQUA_EDIT_LINES=on` (card OA6-02, ED-07: the edit-format bake-off puts it beside
/// `fs.edit`), and is first so that it is the last to give way. `fs.outline` exists only with `AQUA_FS_OUTLINE=on` (card OA5-03, CX-17), and then a coding run sees it. With the switch
/// off the host does not list it and nothing here changes. `check.run` and `project.profile` exist only with `AQUA_CHECK_TOOLS=on` (card OA4-05) and are pinned the same way, and so are
/// `code.symbols` and `code.outline`, which exist only with `AQUA_CODE_TOOLS=on` (card OA12-02), `code.deps` and `code.refs` (the same switch, card OA12-03), `code.map` (`AQUA_REPO_MAP=on`) and the four
/// language-server tools (`AQUA_LSP=on` and a server in `data/tools/lsp.json`, card OA12-04):
/// last, so that when the list is full they are the ones that give way to the tools above.
pub const PINNED_WHEN_OFFERED: &[&str] = &["fs.edit_lines", "fs.outline", "check.run", "project.profile", "code.symbols", "code.outline", "code.refs", "code.deps", "code.map", "code.definition", "code.references", "code.hover", "code.document_symbols"];

/// Names that may be dropped to make room for [`PINNED_TOOLS`].
pub const DROPPABLE: &[&str] = &["term.stop", "term.sessions", "term.start", "fs.stat", "fs.copy", "fs.set_attrs", "git.log", "git.show", "git.blame", "git.branch"];

/// The two tools that only work together: `term.start` begins a background job and `term.output` reads it. A run is shown both or neither (card OA5-01, TM-08): `term.start`
/// was dropped for room while `term.output` stayed, which left the model a tool that reads the output of a job it could not start.
pub const BACKGROUND_JOB_PAIR: [&str; 2] = ["term.start", "term.output"];

/// Tools that give way, in this order, when a tool the goal asked for has no room and nothing in [`DROPPABLE`] is left: read helpers that a goal about moving or deleting files needs least.
pub const GIVE_WAY: &[&str] = &["fs.list", "git.diff", "git.status"];

/// The words that say a goal moves or renames something.
const MOVE_WORDS: &[&str] = &["rename", "renames", "renamed", "renaming", "move", "moves", "moved", "moving", "relocate", "relocates", "relocated", "relocating"];
/// The words that say a goal deletes something.
const DELETE_WORDS: &[&str] = &["delete", "deletes", "deleted", "deleting", "remove", "removes", "removed", "removing", "erase", "erased", "erasing", "trash"];
/// How many words after a delete word a file or a folder has to come in ("delete the old file", "remove build/"): further on it is probably an edit of that file ("remove the unused import from main.rs").
const DELETE_REACH: usize = 3;
/// The words that name a file or a folder.
const FILE_NOUNS: &[&str] = &["file", "files", "folder", "folders", "directory", "directories", "dir"];
/// The words that say a goal stages files for a commit.
const STAGE_WORDS: &[&str] = &["stage", "stages", "staged", "staging", "unstage"];
/// The words that say a goal is about a worktree (card OA7-05).
const WORKTREE_WORDS: &[&str] = &["worktree", "worktrees"];
/// The words that say a goal is about merge conflicts (card OA7-05).
const CONFLICT_WORDS: &[&str] = &["conflict", "conflicts", "conflicted"];
/// File extensions that make a word of a goal a file name (`old.rs`, `notes.txt`).
const FILE_EXTENSIONS: &[&str] = &[
    "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "kt", "cs", "cpp", "c", "h", "rb", "php", "toml", "json", "yaml", "yml", "md", "sh", "ps1", "txt", "csv", "log", "bak", "tmp", "lock", "ini", "cfg", "html", "css", "sql",
];

/// The tools a coding run is shown only when its goal asks for them (cards OA5-01, ED-12, TL-06, TM-08): `fs.move` for a goal that renames or moves, `fs.delete` for one that
/// deletes a file or a folder, `git.add` for one that stages. Without them a run could only do these things through `term.run`, which asks and cannot be undone. Every one of them keeps
/// its approval rules; a goal that only edits ("remove the unused import") is not shown `fs.delete`, because a delete word counts only with a file or a folder soon after it.
pub fn extra_tools_for(goal: &str) -> Vec<&'static str> {
    let lower = goal.to_ascii_lowercase();
    let words: Vec<&str> = lower.split_whitespace().map(|w| w.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '\\' | '-'))).trim_end_matches('.')).filter(|w| !w.is_empty()).collect();
    let mut out: Vec<&'static str> = Vec::new();
    if words.iter().any(|w| MOVE_WORDS.contains(w)) {
        out.push("fs.move");
    }
    let names_a_file = |w: &str| FILE_NOUNS.contains(&w) || w.contains('/') || w.contains('\\') || w.rsplit_once('.').is_some_and(|(stem, ext)| !stem.is_empty() && FILE_EXTENSIONS.contains(&ext));
    if words.iter().enumerate().any(|(i, w)| DELETE_WORDS.contains(w) && words.iter().skip(i + 1).take(DELETE_REACH).any(|n| names_a_file(n))) {
        out.push("fs.delete");
    }
    if words.iter().any(|w| STAGE_WORDS.contains(w)) || lower.contains("git add") {
        out.push("git.add");
    }
    // The tools that wait for a switch (card OA7-05): a goal about a worktree, a conflict or a pull request is shown them when the host lists them (with `AQUA_WORKTREES` or `AQUA_GIT_REVIEW` off it
    // does not, and nothing here changes).
    if words.iter().any(|w| WORKTREE_WORDS.contains(w)) {
        out.extend(["git.worktree_add", "git.worktree_list", "git.worktree_remove"]);
    }
    if words.iter().any(|w| CONFLICT_WORDS.contains(w)) {
        out.extend(["git.conflicts", "git.resolve_conflict"]);
    }
    if lower.contains("pull request") || words.contains(&"pr") {
        out.push("git.pr_draft");
    }
    out
}

/// Hint appended when an edit did not land, so the next step reads and anchors on one span.
pub fn edit_failure_hint(tool: &str, observation: &str) -> Option<&'static str> {
    if !matches!(tool, "fs.edit" | "fs.patch") {
        return None;
    }
    let failed = observation.starts_with("ERROR") || observation.contains("ERROR");
    if failed && (observation.contains("once") || observation.contains("match") || observation.contains("not found") || observation.contains("ambiguous")) {
        return Some("Read the file, then edit again with one exact span that occurs once. Do not overwrite the whole file unless it is new or the person asked to replace it.");
    }
    None
}

/// After a successful write or edit, tell the model which local check to run. Nothing is started here.
pub fn check_hint(tool: &str, observation: &str, path: &str) -> Option<String> {
    if !matches!(tool, "fs.edit" | "fs.edit_lines" | "fs.write" | "fs.patch") {
        return None;
    }
    if observation.starts_with("ERROR") || observation.contains("\nERROR") {
        return None;
    }
    // The command is the one the loop runs after an edit (`coding_runner::check_for`), from the one table of languages (card OA4-04, CU-06).
    let language = aqua_native_tools::code::language_of(path)?;
    let (program, args) = language.edit_check(path)?;
    let cmd = match language {
        aqua_native_tools::code::Language::Python => format!("term.run exe {program}, args [{}, the file you changed]", args[..args.len() - 1].iter().map(|a| format!("\"{a}\"")).collect::<Vec<_>>().join(", ")),
        aqua_native_tools::code::Language::Rust => format!("term.shell command \"{program} {}\" (do not rebuild target/release or the running Aqua core)", args.join(" ")),
        _ => format!("term.shell command \"{program} {}\"", args.join(" ")),
    };
    Some(format!("Change recorded. Next, run one check: {cmd}. Pass expect_version from the last full fs.read. Append the error tail if it fails."))
}

/// Observation text the planner should see: the tool result, plus a hint when one applies.
pub fn annotate(tool: &str, observation: &str, path: &str) -> String {
    let mut out = observation.to_string();
    if let Some(hint) = edit_failure_hint(tool, observation) {
        // A miss on a file that was already told to be read again is not told twice: the message of the tool shows the lines that come closest (card OA6-01, ED-03).
        let only_a_miss = tool == "fs.edit" && observation.contains("not found") && !["once", "match", "ambiguous"].iter().any(|w| observation.contains(w));
        let said_before = only_a_miss && !first_hint(tool, path);
        if !observation.contains(hint) && !said_before {
            out.push('\n');
            out.push_str(hint);
        }
    } else if let Some(hint) = check_hint(tool, observation, path) {
        // One next-step hint per edit (card OA5-01, ED-12): when the result already ends with the host's "Check: run ..." line, this one only repeats it in other words (and names `term.shell`,
        // which asks every time, where the coding prompt teaches `term.run`).
        if !observation.contains("Change recorded.") && !observation.contains(crate::modules::code_verification::CHECK_HINT_PREFIX) {
            out.push('\n');
            out.push_str(&hint);
        }
    }
    out
}

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// What a run knows of one file it read or made (card OA6-01, AG-08).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Seen {
    /// The path as the run wrote it (lower case, `/`): what the coding record shows.
    pub shown: String,
    /// The whole file was read by one `fs.read` that started at line 1 and reached the end (or the run wrote the whole file): not a page of it.
    pub complete: bool,
    /// The version (SHA-256) of the file as the run last saw it: from a whole read, or from what an edit or a write of the run answered.
    pub version: Option<String>,
}

/// The most files one run's ledger keeps (the oldest go first), so that a run that reads a whole tree cannot make it grow without end.
const LEDGER_MAX: usize = 512;

/// run -> the files it read or made, in the order it first met them: (the place, what it knows of it).
type Ledger = HashMap<String, Vec<(String, Seen)>>;

fn ledger() -> &'static Mutex<Ledger> {
    static LEDGER: std::sync::OnceLock<Mutex<Ledger>> = std::sync::OnceLock::new();
    LEDGER.get_or_init(|| Mutex::new(HashMap::new()))
}

fn changes() -> &'static Mutex<HashMap<String, Vec<String>>> {
    static CHANGES: std::sync::OnceLock<Mutex<HashMap<String, Vec<String>>>> = std::sync::OnceLock::new();
    CHANGES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// run -> the hints already given for a failed edit (`tool|path`), so that one that did not help is not repeated.
fn hinted() -> &'static Mutex<HashMap<String, HashSet<String>>> {
    static HINTED: std::sync::OnceLock<Mutex<HashMap<String, HashSet<String>>>> = std::sync::OnceLock::new();
    HINTED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The run these reads belong to. A live agent run uses its run id. A test with no run uses the thread, so two runs cannot share a list.
fn run_key() -> String {
    let id = crate::agent_runs::scope::current_run();
    if id.is_empty() {
        format!("thread:{:?}", std::thread::current().id())
    } else {
        id
    }
}

fn unpoisoned<T>(m: &'static Mutex<T>) -> std::sync::MutexGuard<'static, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Forget files read in the previous run. Call once at the start of an agent run.
pub fn reset_reads() {
    forget_run(&run_key());
}

/// Forgets everything this module holds for `run`: the files it read and changed and the hints it was given. Called when the run ends (`RunScope`), so that the tables are as long as
/// the runs that are going on and not as the runs there have been.
pub fn forget_run(run: &str) {
    unpoisoned(ledger()).remove(run);
    unpoisoned(changes()).remove(run);
    unpoisoned(hinted()).remove(run);
}

/// True the first time this run is told this hint for this file.
fn first_hint(tool: &str, path: &str) -> bool {
    let run = run_key();
    unpoisoned(hinted()).entry(run).or_default().insert(format!("{tool}|{}", canonical(path)))
}

/// How many runs the ledger holds files for (for tests).
pub fn runs_in_ledger() -> usize {
    unpoisoned(ledger()).len()
}

/// Whether the ledger or the record of changes still holds anything for `run` (for tests).
pub fn holds_anything_for(run: &str) -> bool {
    unpoisoned(ledger()).contains_key(run) || unpoisoned(changes()).contains_key(run) || unpoisoned(hinted()).contains_key(run)
}

fn norm(path: &str) -> String {
    path.trim().replace('\\', "/").trim_start_matches("./").to_ascii_lowercase()
}

/// The place `path` names, as one string: where the file tools resolve it (their path guard knows `@ws/...`, relative paths and drive letters, and follows links), with `/` and in lower
/// case. Where the guard does not know the path (no file tools, a place outside every root) it is the path as written. Two names for one file are one place; two files that only
/// share their last name are two.
pub fn canonical(path: &str) -> String {
    canonical_with(crate::tool_host::project_guard().as_deref(), path)
}

/// [`canonical`] with the guard given (none: the path as written, normalised). Two spellings of one file (`@ws/a.rs`, `a.rs` in the first folder, its full path, with any case and slash) are
/// one place; two files that share their last names are two (card OA5-03, CU-13).
pub fn canonical_with(guard: Option<&aqua_native_tools::fs::PathGuard>, path: &str) -> String {
    let raw = path.trim();
    let resolved = guard.and_then(|g| g.resolve(raw, aqua_native_tools::fs::Op::Read).ok()).map(|r| r.path.to_string_lossy().to_string());
    norm(&resolved.unwrap_or_else(|| raw.to_string()))
}

/// Remember a file the run has read in full, so a later edit has an anchor.
pub fn note_read(path: &str) {
    note_read_result(path, true, None);
}

/// Remember a file the run has read: `complete` when it saw the whole file (one read from the first line to the end), and `version` when the read said what version of the file it was.
pub fn note_read_result(path: &str, complete: bool, version: Option<&str>) {
    note(path, complete, version);
}

/// Remember a file the run made or changed (a write, an edit, a patch): its version as the tool answered it. A file the run created whole (`whole`) counts as read in full.
pub fn note_made(path: &str, version: Option<&str>, whole: bool) {
    note(path, whole, version);
}

fn note(path: &str, complete: bool, version: Option<&str>) {
    let place = canonical(path);
    if place.is_empty() {
        return;
    }
    let run = run_key();
    let mut map = unpoisoned(ledger());
    let files = map.entry(run).or_default();
    let at = match files.iter().position(|(p, _)| p == &place) {
        Some(i) => i,
        None => {
            if files.len() >= LEDGER_MAX {
                files.remove(0);
            }
            files.push((place, Seen { shown: norm(path), complete: false, version: None }));
            files.len() - 1
        }
    };
    let seen = &mut files[at].1;
    // A page read after the whole file does not make the whole file a page.
    seen.complete |= complete;
    if let Some(v) = version.filter(|v| !v.is_empty()) {
        seen.version = Some(v.to_string());
    }
}

/// Remember a file this run changed, so the coding record can name it after the observation is shortened.
pub fn note_change(path: &str) {
    let key = norm(path);
    if key.is_empty() {
        return;
    }
    let run = run_key();
    let mut map = unpoisoned(changes());
    let list = map.entry(run).or_default();
    if !list.iter().any(|p| p == &key) {
        list.push(key);
    }
}

/// Files this run has read and changed. Empty when the run has done neither.
pub fn files_this_run() -> (Vec<String>, Vec<String>) {
    let run = run_key();
    let reads = unpoisoned(ledger()).get(&run).map(|files| files.iter().map(|(_, s)| s.shown.clone()).collect()).unwrap_or_default();
    let changed = unpoisoned(changes()).get(&run).cloned().unwrap_or_default();
    (reads, changed)
}

/// What this run knows of the file `path` names (the place, not the last name).
pub fn seen(path: &str) -> Option<Seen> {
    let place = canonical(path);
    let run = run_key();
    unpoisoned(ledger()).get(&run).and_then(|files| files.iter().find(|(p, _)| p == &place).map(|(_, s)| s.clone()))
}

/// A file a call names: the spellings it may be known by, and whether the call creates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    /// The path as the call wrote it; for a diff also the other form (with and without git's `a/` or `b/`): whichever the run has read is the file.
    pub paths: Vec<String>,
    pub creates: bool,
}

/// The files a call of `fs.edit`, `fs.write` or `fs.patch` changes: `path` for the first two; `edits[].path` for a patch with edits; the files named by the `---`/`+++` lines (without git's `a/` and
/// `b/`, and not `/dev/null`) for a patch that is a diff, a file that is created by it marked as such. `Err` with the reason when a diff cannot be read (the gate then refuses: it does not
/// let through what it cannot see).
pub fn named_paths(tool: &str, arguments: &serde_json::Value) -> Result<Vec<Named>, String> {
    let one = |p: &str, creates: bool| Named { paths: vec![p.to_string()], creates };
    match tool {
        "fs.edit" | "fs.edit_lines" | "fs.write" => Ok(vec![one(arguments.get("path").and_then(|v| v.as_str()).unwrap_or(""), false)]),
        "fs.patch" => {
            if let Some(edits) = arguments.get("edits").and_then(|v| v.as_array()) {
                let mut seen_paths: Vec<String> = Vec::new();
                let mut out = Vec::new();
                for e in edits {
                    let p = e.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    if !seen_paths.iter().any(|q| q == p) {
                        seen_paths.push(p.to_string());
                        out.push(one(p, false));
                    }
                }
                return Ok(out);
            }
            let diff = arguments.get("diff").and_then(|v| v.as_str()).unwrap_or("");
            let files = aqua_native_tools::fs::patch::parse_unified(diff)?;
            let mut out: Vec<Named> = Vec::new();
            for f in &files {
                let raw = f.target().to_string();
                let bare = raw.strip_prefix("a/").or_else(|| raw.strip_prefix("b/")).unwrap_or(&raw).to_string();
                let paths = if bare == raw { vec![raw] } else { vec![bare, raw] };
                if !out.iter().any(|n| n.paths == paths) {
                    out.push(Named { paths, creates: f.creates_file() });
                }
            }
            Ok(out)
        }
        _ => Ok(Vec::new()),
    }
}

/// Every path a call of one of the change tools names, as the call wrote it (a created file included): the list `block_unread_change` goes through. An empty list when the tool is none
/// of them or a diff cannot be read.
pub fn paths_of(tool: &str, arguments: &serde_json::Value) -> Vec<String> {
    named_paths(tool, arguments).unwrap_or_default().into_iter().map(|n| n.paths.into_iter().next().unwrap_or_default()).collect()
}

/// The first line of the text of a read: `lines 1-62 of 300 (continue with offset_line=63); 18340 bytes; version <sha>`, or `(empty file); 0 bytes; version <sha>`. The text of a file
/// is inside a fence (a line `<untrusted:...>` comes first), so the line is looked for among the first few.
fn read_header(observation: &str) -> Option<&str> {
    observation.lines().take(3).find(|l| l.starts_with("lines ") || l.starts_with("(empty file)"))
}

/// The version of a file as the text of an `fs.read` says it: in the first line of the page (every read of it names the version of the whole file), or, in the text of an older tool, in a last
/// line `version <64 hex>` that only a whole read had.
pub fn version_in_text(observation: &str) -> Option<String> {
    let hex = |v: &str| (v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit())).then(|| v.to_string());
    if let Some(v) = read_header(observation).and_then(|h| h.rsplit_once("; version ")).and_then(|(_, v)| hex(v.trim())) {
        return Some(v);
    }
    let line = observation.lines().rev().find(|l| l.starts_with("version "))?;
    hex(line["version ".len()..].trim())
}

/// Does the text of an `fs.read` say that the read was the whole file (`(whole file)` in its first line)? `None` when the text is not a page of the new kind: then the caller goes by
/// what it knew before the first line was there.
pub fn read_is_whole(observation: &str) -> Option<bool> {
    let header = aqua_native_tools::fs::page::Header::parse(read_header(observation)?)?;
    Some(header.status == aqua_native_tools::fs::page::Status::Whole)
}

/// What a file tool's answer teaches the run about files: a read says which file it saw, whole or in part, and what version; a write or an edit says the new version of what it made; a patch
/// says the same for each file it changed. `data` is what the tool answered besides the text (null when the caller has none: then the text is read for the version).
pub fn note_result(tool: &str, arguments: &serde_json::Value, observation: &str, data: &serde_json::Value) {
    let arg_path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let version_of = |d: &serde_json::Value| d.get("version").and_then(|v| v.as_str()).map(str::to_string);
    match tool {
        "fs.read" if !arg_path.is_empty() => {
            let from_text = version_in_text(observation);
            let complete = data.get("complete").and_then(|v| v.as_bool()).unwrap_or(from_text.is_some());
            let version = version_of(data).or(from_text);
            // A whole file knows its version whatever the tool says about partial reads; a page that does not carry one adds none.
            note_read_result(arg_path, complete, version.as_deref());
        }
        "fs.edit" | "fs.edit_lines" if !arg_path.is_empty() => {
            note_change(arg_path);
            note_made(arg_path, version_of(data).as_deref(), false);
        }
        "fs.write" if !arg_path.is_empty() => {
            if arguments.get("kind").and_then(|v| v.as_str()) == Some("dir") {
                return;
            }
            note_change(arg_path);
            let mode = arguments.get("mode").and_then(|v| v.as_str()).unwrap_or("create");
            let created = data.get("created").and_then(|v| v.as_bool()).unwrap_or(mode == "create");
            // A file made whole by this run, or written over whole, is known whole; an append adds to what the run knows.
            note_made(arg_path, version_of(data).as_deref(), created || mode == "overwrite");
        }
        "fs.patch" => {
            let files = data.get("files").and_then(|v| v.as_array());
            match files {
                Some(files) => {
                    for f in files {
                        let path = f.get("path").and_then(|v| v.as_str()).unwrap_or("");
                        if !path.is_empty() && f.get("changed").and_then(|v| v.as_bool()) != Some(false) {
                            note_change(path);
                            if f.get("op").and_then(|v| v.as_str()) == Some("create") {
                                note_made(path, None, true);
                            }
                        }
                    }
                    for v in data.get("version").and_then(|v| v.as_array()).into_iter().flatten() {
                        let (path, version) = (v.get("path").and_then(|p| p.as_str()).unwrap_or(""), v.get("version").and_then(|p| p.as_str()));
                        if !path.is_empty() {
                            note_made(path, version, false);
                        }
                    }
                }
                // No data: the files the call names are the files it changed.
                None => {
                    for p in paths_of(tool, arguments) {
                        note_change(&p);
                    }
                }
            }
        }
        _ => {}
    }
}

/// With `expect_version` left out, the version the run last saw of the file is put in: an edit or an overwrite then stops, with the host's own `FILE_CHANGED`, when somebody changed the file
/// after the run read it (the person in an editor, a formatter, a command of the run). Only for a coding goal and only when the run knows a version. Says whether it put one in.
pub fn inject_expect_version(goal: &str, tool: &str, arguments: &mut serde_json::Value) -> bool {
    if !coding_latched(goal) || !matches!(tool, "fs.edit" | "fs.write") {
        return false;
    }
    if tool == "fs.write" && arguments.get("mode").and_then(|v| v.as_str()) != Some("overwrite") {
        return false;
    }
    if arguments.get("expect_version").and_then(|v| v.as_str()).is_some_and(|v| !v.is_empty()) {
        return false;
    }
    let path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let Some(version) = seen(path).and_then(|s| s.version) else { return false };
    if let Some(obj) = arguments.as_object_mut() {
        obj.insert("expect_version".into(), serde_json::Value::String(version));
        return true;
    }
    false
}

/// How many source files the goal names. Two or more means the first planner turn only reads.
pub fn named_code_files(goal: &str) -> usize {
    const EXT: &[&str] = &["rs", "py", "ts", "tsx", "js", "jsx", "go", "toml", "json", "md"];
    let mut n = 0usize;
    for token in goal.split_whitespace() {
        let t = token.trim_matches(|c: char| matches!(c, ',' | ';' | ':' | '"' | '\'' | '(' | ')' | '`')).trim_end_matches('.');
        let Some((stem, ext)) = t.rsplit_once('.') else { continue };
        if !stem.is_empty() && EXT.contains(&ext.to_ascii_lowercase().as_str()) {
            n += 1;
        }
    }
    n
}

/// On the first planner turn of a multi-file coding goal, writes and commands wait until the code has been read.
pub fn read_only_first_turn(goal: &str, tool: &str, planner_rounds: u32) -> Option<&'static str> {
    if planner_rounds > 1 || !coding(goal) || named_code_files(goal) < 2 {
        return None;
    }
    if matches!(tool, "fs.write" | "fs.edit" | "fs.edit_lines" | "fs.patch" | "fs.delete" | "fs.move" | "term.run" | "term.start") {
        return Some("ERROR [PLAN_FIRST] read and search the named files before changing them. This goal names more than one file.");
    }
    None
}

/// Block an edit or a full overwrite until the file has been read, unless the person asked to create or replace it.
///
/// Every file the call names counts: a patch is held to the rule file by file (a file it creates needs no read), and one whose files cannot be told is refused. A file counts as read
/// by the place it is, not by its last name. An overwrite needs the whole file read; an edit or a patch with edits needs it too, unless each says with `expect_count` how many places it
/// changes (a model that read a page of a long file may still change what it saw there). The text a model shows in `old` has to be in the file once anyway; what the read protects is
/// the model's picture of the rest of the file.
pub fn block_unread_change(goal: &str, tool: &str, arguments: &serde_json::Value) -> Option<String> {
    // (`fs.edit_lines`, card OA6-02: the file must have been read, in part at least; the anchor of the call itself holds it to what the model saw, so a partial read is no reason to refuse it.)
    if !coding_latched(goal) || !matches!(tool, "fs.edit" | "fs.edit_lines" | "fs.write" | "fs.patch") {
        return None;
    }
    if tool == "fs.write" && arguments.get("kind").and_then(|v| v.as_str()) == Some("dir") {
        return None;
    }
    // A new file is mode=create (the default): the write tool itself refuses if the path already exists.
    // The goal text is not a bypass. "create a test" must not overwrite an existing file that was never read.
    if tool == "fs.write" {
        let mode = arguments.get("mode").and_then(|v| v.as_str()).unwrap_or("create");
        if mode == "create" {
            return None;
        }
    }
    let named = match named_paths(tool, arguments) {
        Ok(named) => named,
        Err(why) => return Some(format!("ERROR [READ_FIRST] {tool}: the diff could not be read ({why}); correct it, and read each file it changes with fs.read first.")),
    };
    for n in &named {
        if n.creates {
            continue;
        }
        let first = n.paths.first().map(String::as_str).unwrap_or("");
        if first.is_empty() {
            continue;
        }
        let Some(known) = n.paths.iter().find_map(|p| seen(p)) else {
            return Some(format!("ERROR [READ_FIRST] {tool}: read {first} with fs.read before changing it. A new file uses fs.write with mode=create. Overwrite and edit wait until this run has read the file."));
        };
        if !known.complete {
            if let Some(refusal) = partial_read_refusal(tool, arguments, first) {
                return Some(refusal);
            }
        }
    }
    None
}

/// What to say to a call that changes a file the run has read only a page of, or nothing when the call may go on.
fn partial_read_refusal(tool: &str, arguments: &serde_json::Value, path: &str) -> Option<String> {
    let says_how_many = |v: &serde_json::Value| v.get("expect_count").and_then(|c| c.as_u64()).is_some();
    match tool {
        "fs.write" if arguments.get("mode").and_then(|v| v.as_str()) == Some("overwrite") => {
            Some(format!("ERROR [READ_FIRST] fs.write: you have read only part of {path}. Read all of it with one fs.read (leave offset_line out; max_chars shows more of it at once) before replacing it, or change just the part you need with fs.edit."))
        }
        "fs.edit" if !says_how_many(arguments) => {
            Some(format!("ERROR [READ_FIRST] fs.edit: you have read only part of {path}. Read all of it with one fs.read (leave offset_line out), or repeat this edit with expect_count (the number of places it changes, usually 1) to change it from the part you saw."))
        }
        "fs.patch" => {
            let edits = arguments.get("edits").and_then(|v| v.as_array())?;
            let blind = edits.iter().any(|e| e.get("path").and_then(|p| p.as_str()) == Some(path) && !says_how_many(e));
            blind.then(|| format!("ERROR [READ_FIRST] fs.patch: you have read only part of {path}. Read all of it with one fs.read (leave offset_line out), or repeat the patch with expect_count (the number of places an edit changes, usually 1) in each edit of it."))
        }
        _ => None,
    }
}

/// Whether a coding run may call a remote model, and where. Never: the switches that used to record an address here are retired (card OA1-03), so no environment setting
/// makes this answer with one. This function does not connect and does not read the environment.
pub fn remote_model_endpoint() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_coding_goal_uses_the_session_rules_without_the_process_switches() {
        std::env::remove_var("AQUA_CONTEXT");
        std::env::remove_var("AQUA_PLAN");
        std::env::remove_var("AQUA_ASK");
        let goal = "fix the bug in app.rs and run the tests";
        assert!(use_context_v2(goal));
        assert!(wants_plan(goal));
        assert!(allow_ask(goal));
        assert!(!use_context_v2("what is the weather"));
        assert!(!wants_plan("what is the weather"));
        assert!(!allow_ask("open notepad"));
    }

    #[test]
    fn an_edit_waits_until_the_file_was_read_unless_the_request_creates_it() {
        reset_reads();
        let goal = "fix the bug in app.rs";
        let edit = serde_json::json!({"path": "src/app.rs", "old": "a", "new": "b"});
        assert!(block_unread_change(goal, "fs.edit", &edit).unwrap().contains("READ_FIRST"));
        note_read("src\\app.rs");
        assert!(block_unread_change(goal, "fs.edit", &edit).is_none());
        reset_reads();
        assert!(block_unread_change("fix the bug by creating a new file app.rs", "fs.write", &serde_json::json!({"path": "app.rs"})).is_none());
        assert!(block_unread_change("fix the bug and also overwrite app.rs", "fs.write", &serde_json::json!({"path": "src/app.rs", "mode": "overwrite"})).unwrap().contains("READ_FIRST"));
        assert!(block_unread_change(goal, "fs.write", &serde_json::json!({"path": "src", "kind": "dir"})).is_none());
        assert_eq!(named_code_files("fix the bug in src/a.rs and src/b.rs"), 2);
        assert!(read_only_first_turn("fix the bug in src/a.rs and src/b.rs", "fs.edit", 1).is_some());
        assert!(read_only_first_turn("fix the bug in src/a.rs", "fs.edit", 1).is_none());
        assert!(read_only_first_turn("fix the bug in src/a.rs and src/b.rs", "fs.read", 1).is_none());
        assert!(block_unread_change("open notepad", "fs.write", &serde_json::json!({"path": "a.txt"})).is_none());
    }

    /// Card OA6-02: a line edit is held to the loop's rules for an edit: the file must have been read (a page is enough: the anchor of the call holds it to what the model saw), the run knows
    /// the new version of the file after it, and the files it changed are the files it names.
    #[test]
    fn a_line_edit_waits_for_a_read_and_a_page_is_enough_and_its_result_is_noted() {
        reset_reads();
        let goal = "fix the bug in app.rs";
        let by_lines = serde_json::json!({"path": "src/app.rs", "start": 3, "end": 3, "new": "b\n", "expect_first_line": "a"});
        assert!(block_unread_change(goal, "fs.edit_lines", &by_lines).unwrap().contains("READ_FIRST"));
        assert_eq!(paths_of("fs.edit_lines", &by_lines), vec!["src/app.rs".to_string()]);
        // A read of one page: an edit by text is held to say how many places it changes, an edit by lines is not (its anchor is what proves it looked).
        note_read_result("src/app.rs", false, Some("a".repeat(64).as_str()));
        let by_text = serde_json::json!({"path": "src/app.rs", "old": "a", "new": "b"});
        assert!(block_unread_change(goal, "fs.edit", &by_text).unwrap().contains("only part"));
        assert!(block_unread_change(goal, "fs.edit_lines", &by_lines).is_none());
        // What the call answered teaches the run the new version, and the file is one this run changed.
        let v2 = "b".repeat(64);
        note_result("fs.edit_lines", &by_lines, "Replaced line 3 with 1 line", &serde_json::json!({"version": v2}));
        assert_eq!(seen("src/app.rs").and_then(|s| s.version), Some(v2));
        assert!(files_this_run().1.iter().any(|p| p.ends_with("app.rs")), "{:?}", files_this_run());
        // The first turn of a goal that names two files only reads, for this tool as for the others; a check is named after a change.
        assert!(read_only_first_turn("fix the bug in src/a.rs and src/b.rs", "fs.edit_lines", 1).is_some());
        assert!(check_hint("fs.edit_lines", "Replaced line 3 with 1 line", "src/lib.rs").is_some_and(|h| h.contains("cargo check")));
        assert!(check_hint("fs.edit_lines", "ERROR [CONFLICT] fs.edit_lines: the anchor does not match", "src/lib.rs").is_none());
        // It is offered to a coding run beside the others when the host lists it, and first among those that wait for a switch.
        assert_eq!(PINNED_WHEN_OFFERED.first(), Some(&"fs.edit_lines"));
        // The code verification counts it as a change of code.
        let step = |tool: &str| (tool.to_string(), by_lines.clone(), "Replaced line 3".to_string());
        assert!(crate::modules::code_verification::run_is_coding(&[step("fs.edit_lines")]));
        assert!(crate::modules::code_verification::is_code_change("fs.edit_lines"));
        reset_reads();
    }

    #[test]
    fn two_runs_do_not_share_the_files_they_have_read() {
        let a = crate::agent_runs::scope::RunScope::begin(None);
        reset_reads();
        note_read("src/only_a.rs");
        drop(a);
        let b = crate::agent_runs::scope::RunScope::begin(None);
        reset_reads();
        let goal = "fix the bug in only_a.rs";
        let edit = serde_json::json!({"path": "src/only_a.rs", "old": "a", "new": "b"});
        assert!(block_unread_change(goal, "fs.edit", &edit).unwrap().contains("READ_FIRST"));
        drop(b);
    }

    #[test]
    fn a_failed_edit_is_told_to_read_and_a_rust_write_is_told_to_check() {
        let fail = annotate("fs.edit", "ERROR old text must occur once", "src/a.rs");
        assert!(fail.contains("occur once") && fail.contains("Read the file"));
        let ok = annotate("fs.write", "wrote 12 bytes", "src/a.rs");
        assert!(ok.contains("cargo check") && ok.contains("target/release"));
        let py = annotate("fs.edit", "replaced 1", "lib/x.py");
        assert!(py.contains("py_compile"));
        assert_eq!(annotate("fs.read", "line 1", "src/a.rs"), "line 1");
    }

    /// Card OA5-01 (ED-12): a result that already ends with the host's "Check: run ..." line gets no second hint from here, so an edit has one next step and not two that disagree.
    #[test]
    fn one_hint_per_edit() {
        let host = format!("replaced 1\n{}cargo test --offline and read the exit code before you say this is fixed.", crate::modules::code_verification::CHECK_HINT_PREFIX);
        assert_eq!(annotate("fs.edit", &host, "src/a.rs"), host, "the host's line is the one");
        // A result without it still gets this module's hint, once, however many times it is annotated.
        let once = annotate("fs.edit", "replaced 1", "src/a.rs");
        assert_eq!(once.matches("Change recorded.").count(), 1);
        assert_eq!(annotate("fs.edit", &once, "src/a.rs"), once);
        // And what the host adds to a file with no check of its own (notes) is still nothing at all.
        assert_eq!(annotate("fs.edit", "replaced 1", "notes.md"), "replaced 1");
        // The failure hint is not affected: it is not a next step after a change.
        assert!(annotate("fs.edit", &format!("ERROR old text must occur once\n{}", crate::modules::code_verification::CHECK_HINT_PREFIX), "src/a.rs").contains("Read the file"));
    }

    /// Card OA5-01 (ED-12, TL-06): the tools a goal asks for by its words, and none for a goal that only edits.
    #[test]
    fn a_goal_that_renames_deletes_or_stages_is_shown_the_tool_for_it_and_a_plain_edit_is_not() {
        let table: [(&str, &[&str]); 14] = [
            ("rename util.rs to helpers.rs and fix the imports", &["fs.move"]),
            ("move the config file into the settings folder", &["fs.move"]),
            ("Relocate src/old to src/new", &["fs.move"]),
            ("delete the unused file src/old.rs and run cargo build", &["fs.delete"]),
            ("remove the build folder and run cargo build", &["fs.delete"]),
            ("delete old.txt", &["fs.delete"]),
            ("remove src/generated/, then cargo build", &["fs.delete"]),
            ("rename a.rs to b.rs, delete the old file and stage both", &["fs.move", "fs.delete", "git.add"]),
            ("stage lib.rs and commit it", &["git.add"]),
            ("fix the failing test in rust_probe", &[]),
            ("remove the unused import from main.rs", &[]),
            ("delete the debug line in main.rs and run the tests", &[]),
            ("implement caching in utils.py", &[]),
            ("refactor the parser module and run cargo test", &[]),
        ];
        for (goal, want) in table {
            assert_eq!(extra_tools_for(goal), want.to_vec(), "{goal}");
        }
    }

    /// Card OA7-05: a goal about a worktree, a conflict or a pull request is shown the tools for it (when the host lists them), and the goals above are not changed by it.
    #[test]
    fn a_goal_about_a_worktree_a_conflict_or_a_pull_request_is_shown_the_tools_for_it() {
        let table: [(&str, &[&str]); 7] = [
            ("work in a separate worktree and fix the bug in parser.rs", &["git.worktree_add", "git.worktree_list", "git.worktree_remove"]),
            ("clean up the worktrees you made", &["git.worktree_add", "git.worktree_list", "git.worktree_remove"]),
            ("resolve the merge conflict in src/lib.rs", &["git.conflicts", "git.resolve_conflict"]),
            ("fix the conflicts after the merge, then stage lib.rs", &["git.add", "git.conflicts", "git.resolve_conflict"]),
            ("write a pull request description for this branch", &["git.pr_draft"]),
            ("draft the PR text", &["git.pr_draft"]),
            ("the expression a == b is wrong in util.rs", &[]),
        ];
        for (goal, want) in table {
            assert_eq!(extra_tools_for(goal), want.to_vec(), "{goal}");
        }
    }

    /// Card OA5-01: the lists are what the router and the pinning read; a name in two of them, or one that is not a tool of the host, would be found here first.
    #[test]
    fn the_lists_of_tools_name_real_tools_once() {
        for name in PINNED_TOOLS.iter().chain(PINNED_WHEN_OFFERED).chain(DROPPABLE).chain(GIVE_WAY) {
            assert!(name.split_once('.').is_some_and(|(pack, tool)| !pack.is_empty() && !tool.is_empty()), "{name}");
        }
        // Nothing a goal asks for is also something that can be dropped to make room for the pinned tools, and the pair is dropped together or not at all.
        for name in extra_tools_for("rename a.rs to b.rs, delete the old file and stage both") {
            assert!(!DROPPABLE.contains(&name), "{name}");
        }
        assert!(BACKGROUND_JOB_PAIR.contains(&"term.start") && BACKGROUND_JOB_PAIR.contains(&"term.output"));
        assert!(GIVE_WAY.iter().all(|n| PINNED_TOOLS.contains(n)), "only a pinned read helper gives way to a tool the goal asked for");
    }

    /// Card OA1-03: a remote address set in the environment is never handed on (it used to be returned when the switch and an https URL were both set).
    #[test]
    fn a_remote_coding_address_in_the_environment_is_never_recorded() {
        let _env = crate::modules::planner::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let saved: Vec<_> = crate::modules::coding_runner::RETIRED_REMOTE_SWITCHES.iter().map(|n| (*n, std::env::var_os(n))).collect();
        std::env::set_var("AQUA_REMOTE_CODING", "on");
        std::env::set_var("AQUA_REMOTE_CODING_SEND", "on");
        std::env::set_var("AQUA_REMOTE_CODING_URL", "https://example.test/v1");
        let answer = remote_model_endpoint();
        for (name, old) in saved {
            match old {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
        assert_eq!(answer, None);
    }

    // ---- card OA6-01 (AG-08, ED-04): the ledger knows places and versions, and a patch is held like an edit ------------------------------------------------------

    fn edit_of(path: &str) -> serde_json::Value {
        serde_json::json!({"path": path, "old": "a", "new": "b"})
    }

    fn overwrite_of(path: &str) -> serde_json::Value {
        serde_json::json!({"path": path, "content": "x", "mode": "overwrite"})
    }

    #[test]
    fn a_partial_read_does_not_unlock_an_overwrite() {
        reset_reads();
        let goal = "fix the bug in app.rs";
        note_read_result("src/app.rs", false, None);
        let refused = block_unread_change(goal, "fs.write", &overwrite_of("src/app.rs")).expect("a page is not the file");
        assert!(refused.contains("READ_FIRST") && refused.contains("only part") && refused.contains("src/app.rs"), "{refused}");
        // An edit from a page is held too, unless it says how many places it changes.
        let held = block_unread_change(goal, "fs.edit", &edit_of("src/app.rs")).expect("held");
        assert!(held.contains("only part") && held.contains("expect_count"), "{held}");
        let counted = serde_json::json!({"path": "src/app.rs", "old": "a", "new": "b", "expect_count": 1});
        assert!(block_unread_change(goal, "fs.edit", &counted).is_none());
        // A patch with edits the same; a patch that is a diff goes by its context lines.
        let patch = serde_json::json!({"edits": [{"path": "src/app.rs", "old": "a", "new": "b"}]});
        assert!(block_unread_change(goal, "fs.patch", &patch).unwrap().contains("only part"));
        let counted = serde_json::json!({"edits": [{"path": "src/app.rs", "old": "a", "new": "b", "expect_count": 1}]});
        assert!(block_unread_change(goal, "fs.patch", &counted).is_none());
        let diff = serde_json::json!({"diff": "--- a/src/app.rs\n+++ b/src/app.rs\n@@ -1 +1 @@\n-a\n+b\n"});
        assert!(block_unread_change(goal, "fs.patch", &diff).is_none());
        // An append adds to a file and replaces nothing: a read of any part is enough.
        assert!(block_unread_change(goal, "fs.write", &serde_json::json!({"path": "src/app.rs", "content": "x", "mode": "append"})).is_none());
        // The whole file read later unlocks all of it, and a page read after that does not take it back.
        note_read_result("src/app.rs", true, Some(&"a".repeat(64)));
        assert!(block_unread_change(goal, "fs.write", &overwrite_of("src/app.rs")).is_none());
        assert!(block_unread_change(goal, "fs.edit", &edit_of("src/app.rs")).is_none());
        note_read_result("src/app.rs", false, None);
        assert!(block_unread_change(goal, "fs.write", &overwrite_of("src/app.rs")).is_none());
        assert_eq!(seen("src/app.rs").unwrap().version, Some("a".repeat(64)), "a page that carries no version leaves the version it had");
        reset_reads();
    }

    #[test]
    fn a_same_named_file_in_another_folder_is_not_read() {
        reset_reads();
        let goal = "fix the bug in mod.rs";
        note_read("src/other/mod.rs");
        let refused = block_unread_change(goal, "fs.edit", &edit_of("src/x/mod.rs")).expect("another folder's mod.rs is not this one");
        assert!(refused.contains("READ_FIRST") && refused.contains("src/x/mod.rs"), "{refused}");
        assert!(block_unread_change(goal, "fs.edit", &edit_of("src/other/mod.rs")).is_none());
        // The last name alone is a place of its own, not a way into every folder.
        note_read("mod.rs");
        assert!(block_unread_change(goal, "fs.edit", &edit_of("src/x/mod.rs")).is_some());
        // Case and the kind of slash do not make another file.
        note_read("src\\X\\MOD.rs");
        assert!(block_unread_change(goal, "fs.edit", &edit_of("src/x/mod.rs")).is_none());
        reset_reads();
    }

    #[test]
    fn the_ledger_is_emptied_when_the_run_ends() {
        let mut ids = Vec::new();
        let before = runs_in_ledger();
        for i in 0..1000 {
            let scope = crate::agent_runs::scope::RunScope::begin(None);
            let id = scope.id().to_string();
            note_read(&format!("src/f{}.rs", i % 7));
            note_read_result("src/big.rs", false, None);
            note_change("src/a.rs");
            annotate("fs.edit", "ERROR [NOT_FOUND] fs.edit: the text to replace was not found", "src/a.rs");
            assert!(holds_anything_for(&id), "the run {i} holds what it read while it goes on");
            drop(scope);
            assert!(!holds_anything_for(&id), "the run {i} left nothing behind");
            ids.push(id);
        }
        assert!(ids.iter().all(|id| !holds_anything_for(id)));
        assert!(runs_in_ledger() < before + 100, "the table does not grow with the number of runs: {before} before, {} after", runs_in_ledger());
    }

    #[test]
    fn a_ledger_that_a_run_fills_with_files_keeps_its_newest_five_hundred_and_twelve() {
        reset_reads();
        for i in 0..(LEDGER_MAX + 40) {
            note_read(&format!("src/file{i}.rs"));
        }
        let (reads, _) = files_this_run();
        assert_eq!(reads.len(), LEDGER_MAX);
        assert!(seen("src/file0.rs").is_none() && seen(&format!("src/file{}.rs", LEDGER_MAX + 39)).is_some());
        reset_reads();
    }

    #[test]
    fn fs_patch_waits_for_a_read_of_every_file_it_names() {
        reset_reads();
        let goal = "fix the bug in a.rs and b.rs";
        let edits = serde_json::json!({"edits": [{"path": "src/a.rs", "old": "x", "new": "y"}, {"path": "src/b.rs", "old": "x", "new": "y"}, {"path": "src/a.rs", "old": "p", "new": "q"}]});
        let refused = block_unread_change(goal, "fs.patch", &edits).expect("nothing was read");
        assert!(refused.contains("READ_FIRST") && refused.contains("src/a.rs"), "{refused}");
        note_read("src/a.rs");
        let refused = block_unread_change(goal, "fs.patch", &edits).expect("b was not read");
        assert!(refused.contains("src/b.rs") && !refused.contains("src/a.rs"), "the first one not read is named: {refused}");
        note_read("src/b.rs");
        assert!(block_unread_change(goal, "fs.patch", &edits).is_none());
        // A diff names its files in its --- / +++ lines, with or without git's a/ and b/.
        let diff = serde_json::json!({"diff": "--- a/src/c.rs\n+++ b/src/c.rs\n@@ -1 +1 @@\n-x\n+y\n"});
        assert!(block_unread_change(goal, "fs.patch", &diff).unwrap().contains("src/c.rs"));
        note_read("src/c.rs");
        assert!(block_unread_change(goal, "fs.patch", &diff).is_none());
        let plain = serde_json::json!({"diff": "--- src/d.rs\n+++ src/d.rs\n@@ -1 +1 @@\n-x\n+y\n"});
        assert!(block_unread_change(goal, "fs.patch", &plain).unwrap().contains("src/d.rs"));
        // A diff that cannot be read is refused: the gate does not let through what it cannot see.
        let refused = block_unread_change(goal, "fs.patch", &serde_json::json!({"diff": "not a diff"})).expect("refused");
        assert!(refused.contains("READ_FIRST") && refused.contains("could not be read"), "{refused}");
        assert_eq!(paths_of("fs.patch", &edits), vec!["src/a.rs".to_string(), "src/b.rs".to_string()]);
        assert_eq!(paths_of("fs.patch", &diff), vec!["src/c.rs".to_string()]);
        reset_reads();
    }

    #[test]
    fn fs_patch_creating_a_file_needs_no_read() {
        reset_reads();
        let goal = "fix the bug in a.rs";
        let create = serde_json::json!({"diff": "--- /dev/null\n+++ b/src/new.rs\n@@ -0,0 +1 @@\n+fn x() {}\n"});
        assert!(block_unread_change(goal, "fs.patch", &create).is_none());
        // A patch that creates one file and changes another: only the change needs the read.
        let both = serde_json::json!({"diff": "--- /dev/null\n+++ b/src/new.rs\n@@ -0,0 +1 @@\n+x\n--- a/src/old.rs\n+++ b/src/old.rs\n@@ -1 +1 @@\n-a\n+b\n"});
        let refused = block_unread_change(goal, "fs.patch", &both).expect("old.rs was not read");
        assert!(refused.contains("src/old.rs") && !refused.contains("src/new.rs"), "{refused}");
        note_read("src/old.rs");
        assert!(block_unread_change(goal, "fs.patch", &both).is_none());
        reset_reads();
    }

    #[test]
    fn what_an_answer_says_decides_whether_a_read_was_the_whole_file_and_what_the_run_made_it_knows_whole() {
        reset_reads();
        let goal = "fix the bug in big.rs";
        let (v1, v2) = ("a".repeat(64), "b".repeat(64));
        // A page: no version, not complete.
        note_result("fs.read", &serde_json::json!({"path": "src/big.rs", "offset_line": 200}), "200\tfn x() {}\n… more lines follow; continue with offset_line=300", &serde_json::json!({"complete": false, "truncated": true, "version": null}));
        assert!(block_unread_change(goal, "fs.edit", &edit_of("src/big.rs")).unwrap().contains("only part"));
        // The whole file, by what the tool says and, with no data, by the line the text ends with.
        note_result("fs.read", &serde_json::json!({"path": "src/big.rs"}), "1\tfn x() {}", &serde_json::json!({"complete": true, "version": v1}));
        assert!(block_unread_change(goal, "fs.edit", &edit_of("src/big.rs")).is_none());
        assert_eq!(seen("src/big.rs").unwrap().version, Some(v1.clone()));
        reset_reads();
        note_result("fs.read", &serde_json::json!({"path": "src/t.rs"}), &format!("1\tfn x() {{}}\nversion {v2}"), &serde_json::Value::Null);
        assert_eq!(seen("src/t.rs"), Some(Seen { shown: "src/t.rs".into(), complete: true, version: Some(v2.clone()) }));
        assert_eq!(version_in_text("1\tx\nversion short"), None);
        // A file the run created is known whole: a part appended after it is no blind change.
        note_result("fs.write", &serde_json::json!({"path": "src/new.rs", "content": "fn a() {}\n"}), "Wrote 10 bytes to src/new.rs.", &serde_json::json!({"created": true, "version": v1, "total_lines": 1}));
        let append = serde_json::json!({"path": "src/new.rs", "content": "fn b() {}\n", "mode": "append"});
        assert!(block_unread_change(goal, "fs.write", &append).is_none());
        assert!(block_unread_change(goal, "fs.write", &overwrite_of("src/new.rs")).is_none());
        // A patch that created a file, and one that changed another: both are named, with the version the patch left.
        note_result(
            "fs.patch",
            &serde_json::json!({"diff": "x"}),
            "Patched 2 file(s)",
            &serde_json::json!({"files": [{"path": "src/p.rs", "op": "create", "changed": true}, {"path": "src/q.rs", "op": "update", "changed": true}, {"path": "src/r.rs", "op": "update", "changed": false}], "version": [{"path": "src/p.rs", "version": v1}, {"path": "src/q.rs", "version": v2}]}),
        );
        assert!(block_unread_change(goal, "fs.edit", &edit_of("src/p.rs")).is_none(), "created by the patch: known whole");
        assert_eq!(seen("src/q.rs").unwrap().version, Some(v2));
        assert!(!seen("src/q.rs").unwrap().complete, "a file the patch changed is not thereby read");
        let (_, changed) = files_this_run();
        assert!(changed.contains(&"src/p.rs".to_string()) && changed.contains(&"src/q.rs".to_string()) && !changed.contains(&"src/r.rs".to_string()), "{changed:?}");
        reset_reads();
    }

    #[test]
    fn an_edit_gets_the_version_the_run_last_saw_unless_it_names_one() {
        reset_reads();
        let goal = "fix the bug in app.rs";
        let (v1, v2) = ("1".repeat(64), "2".repeat(64));
        note_result("fs.read", &serde_json::json!({"path": "src/app.rs"}), "1\tfn a() {}", &serde_json::json!({"complete": true, "version": v1}));
        let mut edit = edit_of("src/app.rs");
        assert!(inject_expect_version(goal, "fs.edit", &mut edit));
        assert_eq!(edit["expect_version"], v1.as_str());
        // One the model named is its own.
        let mut named = serde_json::json!({"path": "src/app.rs", "old": "a", "new": "b", "expect_version": "mine"});
        assert!(!inject_expect_version(goal, "fs.edit", &mut named));
        assert_eq!(named["expect_version"], "mine");
        // After the edit's own answer, the new version is the one: a second edit is not stopped by the first.
        note_result("fs.edit", &serde_json::json!({"path": "src/app.rs"}), "Replaced 1 occurrence(s)", &serde_json::json!({"version": v2}));
        let mut again = edit_of("src/app.rs");
        assert!(inject_expect_version(goal, "fs.edit", &mut again));
        assert_eq!(again["expect_version"], v2.as_str());
        // An overwrite gets one, a create does not, a file the run knows no version of gets none, and a run that is not about code is left alone.
        let mut over = overwrite_of("src/app.rs");
        assert!(inject_expect_version(goal, "fs.write", &mut over) && over["expect_version"] == v2.as_str());
        let mut create = serde_json::json!({"path": "src/app.rs", "content": "x"});
        assert!(!inject_expect_version(goal, "fs.write", &mut create) && create.get("expect_version").is_none());
        let mut other = edit_of("src/other.rs");
        assert!(!inject_expect_version(goal, "fs.edit", &mut other));
        let mut elsewhere = edit_of("src/app.rs");
        assert!(!inject_expect_version("open notepad", "fs.edit", &mut elsewhere));
        reset_reads();
    }

    #[test]
    fn a_miss_on_one_file_is_told_to_read_once_and_a_second_one_is_not_told_again() {
        let miss = "ERROR [NOT_FOUND] fs.edit: the text to replace was not found; the closest lines in the file are 2-4:\n2\tfn target() {";
        let first = annotate("fs.edit", miss, "src/a.rs");
        assert!(first.contains("Read the file, then edit again"), "{first}");
        let second = annotate("fs.edit", miss, "src/a.rs");
        assert_eq!(second, miss, "the same file, the same miss: the tool's own words are enough");
        assert!(annotate("fs.edit", miss, "src/b.rs").contains("Read the file"), "another file is told");
        // A text that matches several places is told every time.
        let several = "ERROR [CONFLICT] fs.edit: the text matches 2 places after trimming; add context to make it unique";
        assert!(annotate("fs.edit", several, "src/a.rs").contains("Read the file"));
        assert!(annotate("fs.edit", several, "src/a.rs").contains("Read the file"));
    }

    // ---- card OA5-03 (CU-13, ED-01): the places of the ledger, and the first line of a page ---------------------------------------------------------------------------

    /// Card OA5-03 (CU-13): a file has one place however it is written, and two files that share their last names are two places.
    #[test]
    fn the_same_file_by_alias_relative_and_absolute_path_is_one_file() {
        use aqua_native_tools::fs::{Access, FsConfig, PathGuard, Root};
        let dir = std::env::temp_dir().join(format!("aqua_canon_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for f in ["ws/src", "other/src"] {
            std::fs::create_dir_all(dir.join(f)).unwrap();
            std::fs::write(dir.join(f).join("mod.rs"), "x").unwrap();
        }
        let root = |alias: &str| Root { alias: alias.into(), path: dir.join(alias).to_string_lossy().into(), access: Access::Rw, trusted: true };
        let guard = PathGuard::new(FsConfig { roots: vec![root("ws"), root("other")], ..FsConfig::with_workspace("unused") }, vec![]).unwrap();
        let place = |p: &str| canonical_with(Some(&guard), p);
        let by_alias = place("@ws/src/mod.rs");
        // Relative to the first folder, in the other kind of slash and with another case, and by its full path: one file.
        let full = dir.join("ws").join("src").join("mod.rs").to_string_lossy().to_string();
        for same in ["src/mod.rs", "src\\MOD.rs", "./src/mod.rs", &full, &full.replace('\\', "/").to_uppercase()] {
            assert_eq!(place(same), by_alias, "{same}");
        }
        // Another folder's mod.rs, and a bare mod.rs (which is in the first folder, and is not there), are other places.
        assert_ne!(place("@other/src/mod.rs"), by_alias);
        assert_ne!(place("mod.rs"), by_alias);
        // With no guard the path is its own place, written one way.
        assert_eq!(canonical_with(None, " Src\\Mod.rs "), "src/mod.rs");
        assert_ne!(canonical_with(None, "src/mod.rs"), canonical_with(None, "other/src/mod.rs"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Card OA5-03 (CU-13): reading `src/mod.rs` does not unlock `other/src/mod.rs`.
    #[test]
    fn a_read_of_one_mod_rs_does_not_unlock_another() {
        reset_reads();
        let goal = "fix the bug in mod.rs";
        note_read("src/mod.rs");
        assert!(block_unread_change(goal, "fs.edit", &edit_of("src/mod.rs")).is_none());
        let refused = block_unread_change(goal, "fs.edit", &edit_of("other/src/mod.rs")).expect("another file");
        assert!(refused.contains("READ_FIRST") && refused.contains("other/src/mod.rs"), "{refused}");
        // Not by a suffix either way: a longer path that ends in the one read, and a shorter one that the one read ends in.
        assert!(block_unread_change(goal, "fs.edit", &edit_of("x/src/mod.rs")).is_some());
        assert!(block_unread_change(goal, "fs.edit", &edit_of("mod.rs")).is_some());
        reset_reads();
    }

    /// Card OA5-03 (ED-01): every read names the version of the whole file in its first line, and the first line says whether the page was the whole file; a run that is told only the
    /// text goes by it.
    #[test]
    fn the_first_line_of_a_page_names_the_version_and_says_whether_it_was_the_whole_file() {
        let v = "c".repeat(64);
        let fenced = |inner: &str| aqua_tool_host::output::label_untrusted(inner, "abc123abc123");
        let whole = fenced(&format!("lines 1-3 of 3 (whole file); 12 bytes; version {v}\n1\ta\n2\tb\n3\tc"));
        assert_eq!((version_in_text(&whole), read_is_whole(&whole)), (Some(v.clone()), Some(true)));
        let part = fenced(&format!("lines 1-2 of 9 (continue with offset_line=3); 99 bytes; version {v}\n1\ta\n2\tb"));
        assert_eq!((version_in_text(&part), read_is_whole(&part)), (Some(v.clone()), Some(false)));
        let end = format!("lines 5-9 of 9 (end of file); 99 bytes; version {v}\n5\ta");
        assert_eq!(read_is_whole(&end), Some(false), "a page that reaches the end is not the whole file");
        // A file too big to have a version says so, and has none.
        let big = "lines 1-5 (continue with offset_line=6); 90000000 bytes; too large to version\n1\ta";
        assert_eq!((version_in_text(big), read_is_whole(big)), (None, Some(false)));
        assert_eq!(version_in_text(&format!("(empty file); 0 bytes; version {v}")), Some(v.clone()));
        // Text that is not a page: nothing is said, and the older form (a last line) is still read.
        assert_eq!(read_is_whole("1\ta\n2\tb"), None);
        assert_eq!(version_in_text(&format!("1\ta\nversion {v}")), Some(v.clone()));
        // A line of the file that looks like a header, further down, is not one.
        assert_eq!(version_in_text(&format!("lines 1-1 of 1 (whole file); 3 bytes\n1\tx; version {v}")), None);
        // With no data (the old route) the run goes by the first line: a page does not unlock an overwrite, and the whole file does.
        reset_reads();
        let goal = "fix the bug in p.rs";
        crate::modules::coding_runner::after_host_data("fs.read", &serde_json::json!({"path": "src/p.rs"}), &part, &serde_json::Value::Null);
        assert!(block_unread_change(goal, "fs.write", &overwrite_of("src/p.rs")).unwrap().contains("only part"));
        assert_eq!(seen("src/p.rs").unwrap().version, Some(v.clone()), "the version of the whole file, from a page");
        crate::modules::coding_runner::after_host_data("fs.read", &serde_json::json!({"path": "src/p.rs"}), &whole, &serde_json::Value::Null);
        assert!(block_unread_change(goal, "fs.write", &overwrite_of("src/p.rs")).is_none());
        reset_reads();
    }
}

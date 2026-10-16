//! The review tools (card OA7-05, TM-17 and TM-18): `git.conflicts`, `git.resolve_conflict` and `git.pr_draft`, and the refusal of a commit while a file is unmerged.
//!
//! They are on offer only with `AQUA_GIT_REVIEW=on`. Nothing here reaches a network and no model is used:
//! * `git.conflicts` lists the unmerged files and the hunks of each text file (what each side has), from `git ls-files -u` and the file itself. Like every git result it is text from
//!   outside, and the host fences it. A file whose name looks like a file of secrets is listed and its text is not shown.
//! * `git.resolve_conflict` replaces the hunks of one file, and only that file, as an edit: the host asks the person with the change as a diff (`resolve_preview`), the file is kept in the
//!   undo store of the file tools first, and the call refuses if the file is not what the person was shown. It stages nothing and commits nothing.
//! * `git.pr_draft` writes a title and a description from the commits and the file names of the branch (`prdraft.rs`).

use super::conflicts::{self, Take};
use super::prdraft;
use super::{arg_str, arg_u64, args, bad, CallCx, Inner, Repo};
use crate::fs::guard::{check_opened_at, display, Op, Resolved};
use aqua_tool_host::{fsutil, ChangePreview, ErrorCode, ProviderOutput, ToolError};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

/// The most conflicted files `git.conflicts` describes in one answer, and the most text.
const MAX_FILES: usize = 20;
const TEXT_BUDGET: usize = 24_000;
/// How much of a side of a hunk is shown: lines, and characters of a line.
const SIDE_LINES: usize = 12;
const SIDE_CHARS: usize = 160;

/// What `git.resolve_conflict` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Choices {
    /// The same for every hunk of the file.
    Every(Take),
    /// Hunk by hunk.
    Hunks(Vec<(usize, Take)>),
}

/// Reads the arguments of `git.resolve_conflict` that say what to do, and refuses what is not one thing or the other.
pub(super) fn parse_choices(a: &Value) -> Result<Choices, ToolError> {
    let all = arg_str(a, "take");
    match (all.is_empty(), a.get("resolutions")) {
        (true, None) => Err(bad("say what to do: take=ours, theirs or both for every hunk of the file, or resolutions=[{hunk, take, text}] hunk by hunk")),
        (false, Some(_)) => Err(bad("give take (the same for every hunk) or resolutions (hunk by hunk), not both")),
        (false, None) => match all {
            "ours" => Ok(Choices::Every(Take::Ours)),
            "theirs" => Ok(Choices::Every(Take::Theirs)),
            "both" => Ok(Choices::Every(Take::Both)),
            other => Err(bad(format!("take is ours, theirs or both for every hunk, not '{other}' (your own text goes with a hunk, in resolutions)"))),
        },
        (true, Some(list)) => {
            let items = list.as_array().ok_or_else(|| bad("resolutions is a list of {hunk, take, text}"))?;
            if items.is_empty() || items.len() > 50 {
                return Err(bad("resolutions names between 1 and 50 hunks"));
            }
            let mut out: Vec<(usize, Take)> = Vec::new();
            let mut written = 0usize;
            for item in items {
                let hunk = item.get("hunk").and_then(|h| h.as_u64()).filter(|h| *h >= 1).ok_or_else(|| bad("each resolution names a hunk (1 is the first hunk of the file)"))? as usize;
                let take = match item.get("take").and_then(|t| t.as_str()) {
                    Some("ours") => Take::Ours,
                    Some("theirs") => Take::Theirs,
                    Some("both") => Take::Both,
                    Some("text") => {
                        let t = item.get("text").and_then(|t| t.as_str()).ok_or_else(|| bad(format!("hunk {hunk}: take=text needs the text")))?;
                        args::text(t, 20_000, "text")?;
                        written += t.len();
                        if written > 60_000 {
                            return Err(bad("the texts of one call are at most 60000 characters"));
                        }
                        Take::Text(t.to_string())
                    }
                    _ => return Err(bad(format!("hunk {hunk}: take is ours, theirs, both or text"))),
                };
                if item.get("text").is_some() && !matches!(take, Take::Text(_)) {
                    return Err(bad(format!("hunk {hunk}: text goes only with take=text")));
                }
                out.push((hunk, take));
            }
            Ok(Choices::Hunks(out))
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// A file's text as a diff compares it: no byte order mark, every line break `\n`.
fn plain(text: &str) -> String {
    crate::fs::patch::lf(text.strip_prefix('\u{feff}').unwrap_or(text))
}

/// The bytes of a file of at most [`conflicts::MAX_FILE_BYTES`] that is text, read from the handle the guard's answer was checked against.
fn read_text(r: &Resolved) -> Result<Vec<u8>, ToolError> {
    let meta = std::fs::metadata(&r.path).map_err(|e| ToolError::new(ErrorCode::NotFound, format!("{} cannot be read ({e})", display(&r.path))))?;
    if !meta.is_file() {
        return Err(bad(format!("{} is not a file", display(&r.path))));
    }
    if meta.len() > conflicts::MAX_FILE_BYTES {
        return Err(ToolError::new(ErrorCode::ResourceLimit, format!("{} is larger than {} KB: it is not read here, resolve it by hand", display(&r.path), conflicts::MAX_FILE_BYTES / 1024)));
    }
    let mut f = std::fs::File::open(&r.path).map_err(|e| ToolError::new(ErrorCode::OsPermissionDenied, format!("{} cannot be opened ({e})", display(&r.path))))?;
    check_opened_at(&f, &r.path)?;
    let mut bytes = Vec::new();
    f.by_ref().take(conflicts::MAX_FILE_BYTES + 1).read_to_end(&mut bytes).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("{} cannot be read ({e})", display(&r.path))))?;
    if bytes[..bytes.len().min(8000)].contains(&0) {
        return Err(bad(format!("{} is a binary file: it is not resolved by hunks (take a whole side by hand)", display(&r.path))));
    }
    Ok(bytes)
}

/// Everything a resolution has worked out before anything is written.
pub(super) struct ResolvePlan {
    rel: String,
    resolved: Resolved,
    old_sha: String,
    old_text: String,
    new_text: String,
    total: usize,
    left: usize,
    taken: Vec<String>,
}

impl ResolvePlan {
    fn line(&self, alias: &str) -> String {
        format!("Resolve {} of {} conflict hunk{} in {} of {alias} ({}); {} left", self.total - self.left, self.total, if self.total == 1 { "" } else { "s" }, self.rel, self.taken.join(", "), self.left)
    }
}

impl Inner {
    /// Are the review tools on (`AQUA_GIT_REVIEW`, asked each time)?
    pub(super) fn review_enabled(&self) -> bool {
        self.review_on.as_ref().is_some_and(|on| on())
    }

    fn need_review(&self) -> Result<(), ToolError> {
        if self.review_enabled() {
            Ok(())
        } else {
            Err(ToolError::new(ErrorCode::Unavailable, "the conflict and pull request tools are off (AQUA_GIT_REVIEW=on turns them on)"))
        }
    }

    /// The paths the index holds as unmerged, with the stages it holds for each (bit 0: the common ancestor, bit 1: ours, bit 2: theirs).
    pub(super) fn unmerged(&self, repo: &Repo, cancel: &AtomicBool) -> Result<Vec<(String, u8)>, ToolError> {
        let r = self.ok(repo, &["ls-files", "-u", "-z"], false, 1024, 30, cancel)?;
        let mut out: Vec<(String, u8)> = Vec::new();
        for entry in r.stdout.split('\0') {
            let Some((meta, path)) = entry.split_once('\t') else { continue };
            let Some(stage) = meta.split_whitespace().nth(2).and_then(|s| s.parse::<u8>().ok()).filter(|s| (1..=3).contains(s)) else { continue };
            let bit = 1u8 << (stage - 1);
            match out.last_mut() {
                Some((p, bits)) if p.as_str() == path => *bits |= bit,
                _ => out.push((path.to_string(), bit)),
            }
        }
        Ok(out)
    }

    /// A commit is not made while a file of the index is unmerged (with the review tools on): git refuses it too, but with words that name no tool.
    pub(super) fn refuse_unmerged(&self, repo: &Repo, cancel: &AtomicBool) -> Result<(), ToolError> {
        let files = self.unmerged(repo, cancel)?;
        if files.is_empty() {
            return Ok(());
        }
        let names: Vec<&str> = files.iter().take(8).map(|(p, _)| p.as_str()).collect();
        Err(ToolError::new(
            ErrorCode::MergeConflict,
            format!("the commit was not made: {} file(s) are still unmerged ({}{}). git.conflicts shows them; resolve each, stage it with git.add, and commit again.", files.len(), names.join(", "), if files.len() > 8 { ", ..." } else { "" }),
        )
        .with_details(json!({"files": files.iter().map(|(p, _)| p).take(50).collect::<Vec<_>>()})))
    }

    /// What is in progress in the repository, from the files git keeps in its folder: a merge, a cherry-pick, a revert or a rebase.
    fn operation(&self, repo: &Repo, cancel: &AtomicBool) -> Option<(&'static str, String)> {
        let r = self.git(repo, &["rev-parse", "--absolute-git-dir"], false, 4, 15, cancel).ok().filter(|r| r.code == Some(0))?;
        let dir = PathBuf::from(r.stdout.trim());
        let first_line = |name: &str| std::fs::read_to_string(dir.join(name)).ok().and_then(|t| t.lines().next().map(|l| l.trim().chars().take(80).collect::<String>())).unwrap_or_default();
        if dir.join("MERGE_HEAD").is_file() {
            return Some(("merge", first_line("MERGE_MSG")));
        }
        if dir.join("CHERRY_PICK_HEAD").is_file() {
            return Some(("cherry-pick", first_line("CHERRY_PICK_HEAD").chars().take(12).collect()));
        }
        if dir.join("REVERT_HEAD").is_file() {
            return Some(("revert", first_line("REVERT_HEAD").chars().take(12).collect()));
        }
        if dir.join("rebase-merge").is_dir() || dir.join("rebase-apply").is_dir() {
            return Some(("rebase", String::new()));
        }
        None
    }

    /// `git.conflicts`.
    pub(super) fn conflicts(&self, repo: &Repo, a: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
        self.need_review()?;
        let only = match arg_str(a, "path") {
            "" => None,
            p => Some(self.rel_path(repo, p, Op::Read)?),
        };
        let max_hunks = arg_u64(a, "max_hunks", 3).clamp(1, 10) as usize;
        let all = self.unmerged(repo, cancel)?;
        let files: Vec<&(String, u8)> = all.iter().filter(|(p, _)| only.as_ref().is_none_or(|o| o == p)).collect();
        let op = self.operation(repo, cancel);
        let mut text = String::new();
        match &op {
            Some((kind, what)) => {
                text.push_str(&format!("A {kind} is in progress{}.", if what.is_empty() { String::new() } else { format!(" ({what})") }));
                text.push_str(match *kind {
                    "merge" => " git.merge action=abort puts everything back as it was before the merge.",
                    _ => " Aqua has no tool to abort or continue it: the person can (git cherry-pick/revert/rebase --abort or --continue).",
                });
                text.push('\n');
            }
            None if !all.is_empty() => text.push_str("No merge, cherry-pick, revert or rebase is in progress, but the index holds unmerged files (a stash that was applied or a checkout --merge leaves them).\n"),
            None => {}
        }
        if files.is_empty() {
            text.push_str(if only.is_some() { "That file is not conflicted." } else if op.is_some() { "No file is conflicted now: stage what was resolved (git.add) and commit to finish." } else { "No conflicts." });
            return Ok(ProviderOutput { data: json!({"repo": repo.entry.alias, "operation": op.as_ref().map(|o| o.0), "files": []}), text, ..Default::default() });
        }
        text.push_str(&format!("{} conflicted file{}:\n", files.len(), if files.len() == 1 { "" } else { "s" }));
        let mut rows = Vec::new();
        for (i, (path, stages)) in files.iter().map(|f| (&f.0, f.1)).enumerate() {
            if i >= MAX_FILES || text.len() > TEXT_BUDGET {
                text.push_str(&format!("... {} more file(s): give path= to see one.\n", files.len() - i));
                break;
            }
            let kind = conflicts::kind_of(stages);
            text.push_str(&format!("\n{}. {path} ({kind})\n", i + 1));
            let mut row = json!({"path": path, "kind": kind, "hunks": 0, "shown": 0});
            let abs = repo.root.join(path);
            let resolved = match self.guard.resolve(&abs.to_string_lossy(), Op::Read) {
                Ok(r) => r,
                Err(e) => {
                    text.push_str(&format!("   not read: {}\n", e.message));
                    rows.push(row);
                    continue;
                }
            };
            if resolved.sensitive {
                text.push_str("   its text is not shown: the name looks like a file of secrets. Resolve it by hand.\n");
                rows.push(row);
                continue;
            }
            let file_text = match read_text(&resolved).and_then(|b| String::from_utf8(b).map_err(|_| bad("it is not UTF-8 text"))) {
                Ok(t) => t,
                Err(e) if e.code == ErrorCode::NotFound => {
                    text.push_str("   the file is not in the folder (one side deleted it): resolve it by hand (git add to keep it, git rm to delete it), or abort.\n");
                    rows.push(row);
                    continue;
                }
                Err(e) => {
                    text.push_str(&format!("   hunks not shown: {}\n", e.message));
                    rows.push(row);
                    continue;
                }
            };
            let parsed = conflicts::parse(&file_text);
            let hunks = parsed.hunks();
            row["hunks"] = json!(hunks.len());
            if let Some(line) = parsed.unbalanced {
                text.push_str(&format!("   the markers do not balance (see line {line}): resolve it by hand.\n"));
                row["unbalanced"] = json!(line);
            }
            if hunks.is_empty() && parsed.unbalanced.is_none() {
                if matches!(stages, 0b111 | 0b110) {
                    text.push_str("   no conflict markers in the file (it may have been resolved by hand): stage it with git.add when it is right.\n");
                } else {
                    text.push_str("   there is nothing to mark: one side deleted the file or only one side has it. Keep it with git add, or delete it with git rm (by hand; Aqua has no tool for it), or abort.\n");
                }
            }
            for h in hunks.iter().take(max_hunks) {
                text.push_str(&format!("   hunk {} (lines {}-{}): ours = {}, theirs = {}{}\n", h.index, h.start, h.end, h.ours_label, h.theirs_label, h.base_label.as_ref().map(|b| format!(", base = {b}")).unwrap_or_default()));
                for (name, side) in [("ours", Some(&h.ours)), ("base", h.base.as_ref()), ("theirs", Some(&h.theirs))] {
                    if let Some(side) = side {
                        text.push_str(&format!("     {name}:\n"));
                        for l in conflicts::excerpt(side, SIDE_LINES, SIDE_CHARS).lines() {
                            text.push_str(&format!("       {l}\n"));
                        }
                    }
                }
            }
            if hunks.len() > max_hunks {
                text.push_str(&format!("   ... {} more hunk(s): max_hunks or path= shows them.\n", hunks.len() - max_hunks));
            }
            row["shown"] = json!(hunks.len().min(max_hunks));
            rows.push(row);
        }
        text.push_str("\nTo resolve one file: git.resolve_conflict path=<file> take=ours|theirs|both, or resolutions=[{hunk, take: ours|theirs|both|text, text}]. The person approves the change as a diff; nothing is staged or committed. Then git.add the file, and git.commit once none is left.");
        Ok(ProviderOutput { data: json!({"repo": repo.entry.alias, "operation": op.as_ref().map(|o| o.0), "files": rows}), text, ..Default::default() })
    }

    /// What a resolution is, worked out from the file as it is now; nothing is written.
    fn plan_resolve(&self, repo: &Repo, a: &Value, cancel: &AtomicBool) -> Result<ResolvePlan, ToolError> {
        self.need_review()?;
        let rel = self.rel_path(repo, arg_str(a, "path"), Op::Write)?;
        let choices = parse_choices(a)?;
        let unmerged = self.unmerged(repo, cancel)?;
        let Some((_, stages)) = unmerged.iter().find(|(p, _)| *p == rel) else {
            return Err(ToolError::new(ErrorCode::NotFound, format!("git does not list '{rel}' as conflicted (repo {}). git.conflicts lists the files that are.", repo.entry.alias)));
        };
        let resolved = self.guard.resolve(&repo.root.join(&rel).to_string_lossy(), Op::Write)?;
        let bytes = read_text(&resolved).map_err(|e| if e.code == ErrorCode::NotFound { bad(format!("'{rel}' is conflicted ({}) and is not in the folder: resolve it with git add (keep it) or git rm (delete it), by hand", conflicts::kind_of(*stages))) } else { e })?;
        let old_sha = sha256_hex(&bytes);
        let old_text = String::from_utf8(bytes).map_err(|_| bad(format!("'{rel}' is not UTF-8 text: it is not resolved by hunks")))?;
        let parsed = conflicts::parse(&old_text);
        if let Some(line) = parsed.unbalanced {
            return Err(bad(format!("the markers of '{rel}' do not balance (see line {line}): resolve it by hand")));
        }
        let hunks = parsed.hunks().len();
        if hunks == 0 {
            return Err(bad(format!("'{rel}' is conflicted ({}) but holds no conflict markers: there is no hunk to resolve. Stage it with git.add if it is right.", conflicts::kind_of(*stages))));
        }
        let list: Vec<(usize, Take)> = match choices {
            Choices::Every(t) => (1..=hunks).map(|n| (n, t.clone())).collect(),
            Choices::Hunks(l) => l,
        };
        let taken: Vec<String> = list.iter().map(|(n, t)| format!("hunk {n}: {}", t.word())).collect();
        let (new_text, left) = conflicts::resolve(&parsed, &list).map_err(bad)?;
        Ok(ResolvePlan { rel, resolved, old_sha, old_text, new_text, total: hunks, left, taken })
    }

    /// The line of the question for `git.resolve_conflict`; a call that cannot be made is refused before the person is asked.
    pub(super) fn resolve_summary(&self, repo: &Repo, a: &Value) -> Result<String, ToolError> {
        let plan = self.plan_resolve(repo, a, &AtomicBool::new(false))?;
        Ok(plan.line(&repo.entry.alias))
    }

    /// The diff the person sees before they approve a resolution, and what it was made from (`ChangePreview::guard`).
    pub(super) fn resolve_preview(&self, repo: &Repo, a: &Value) -> Option<ChangePreview> {
        let plan = self.plan_resolve(repo, a, &AtomicBool::new(false)).ok()?;
        let guard = json!({"git": 1, "path": plan.rel, "sha": plan.old_sha}).to_string();
        if plan.resolved.sensitive {
            return Some(ChangePreview { text: aqua_tool_host::PREVIEW_HIDDEN.to_string(), guard: Some(guard) });
        }
        let name = display(&plan.resolved.path);
        let d = crate::fs::diff::diff(&name, &name, &plain(&plan.old_text), &plain(&plan.new_text), 3, 160);
        Some(ChangePreview { text: format!("{}\n{}", plan.line(&repo.entry.alias), d.text), guard: Some(guard) })
    }

    /// `git.resolve_conflict`: the file is kept in the undo store, then written with the hunks replaced.
    pub(super) fn resolve_conflict(&self, repo: &Repo, a: &Value, cx: &CallCx, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
        let plan = self.plan_resolve(repo, a, cancel)?;
        // The person was shown a diff of a file as it was; if it is not that file any more, nothing is changed.
        if let Some(shown) = cx.shown.as_deref().and_then(|g| serde_json::from_str::<Value>(g).ok()).filter(|v| v.get("git").and_then(|g| g.as_u64()) == Some(1)) {
            if shown.get("path").and_then(|p| p.as_str()) != Some(plan.rel.as_str()) || shown.get("sha").and_then(|s| s.as_str()) != Some(plan.old_sha.as_str()) {
                return Err(ToolError::new(ErrorCode::Conflict, format!("'{}' changed after you were shown the resolution: nothing was written. Call git.conflicts again and propose it again.", plan.rel)));
            }
        }
        let Some(trash) = self.trash.clone() else {
            return Err(ToolError::new(ErrorCode::Unavailable, "git.resolve_conflict needs the undo store of the file tools, which is not there: nothing was changed"));
        };
        let entry = crate::fs::trash::with_run(&cx.run_id, || trash.backup(&plan.resolved.path, "edited")).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("{e}; nothing was changed")))?;
        fsutil::write_atomic_durable(&plan.resolved.path, plan.new_text.as_bytes()).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("{} could not be written ({e}): it is as it was", display(&plan.resolved.path))))?;
        trash.record_after(&entry, &sha256_hex(plan.new_text.as_bytes()));
        let done = plan.total - plan.left;
        let text = format!(
            "Resolved {done} of {} conflict hunk{} in {} ({}).{}\nNothing is staged or committed. When every conflicted file is resolved: git.add the files, then git.commit.\nUndo this run (fs.undo_run, or the app's button) puts the file back as it was, markers included (undo id {}).",
            plan.total,
            if plan.total == 1 { "" } else { "s" },
            plan.rel,
            plan.taken.join(", "),
            if plan.left == 0 { " No conflict marker is left in the file.".to_string() } else { format!(" {} hunk(s) still hold conflict markers.", plan.left) },
            entry.undo_id
        );
        Ok(ProviderOutput { data: json!({"repo": repo.entry.alias, "path": plan.rel, "resolved": done, "left": plan.left, "undo_id": entry.undo_id}), text, ..Default::default() })
    }

    /// The branch a pull request is made against when none is named: where `origin/HEAD` points, else `main`, `master`, `trunk` or `develop`, whichever there is.
    fn default_base(&self, repo: &Repo, cancel: &AtomicBool) -> Option<String> {
        let r = self.git(repo, &["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"], false, 4, 15, cancel).ok()?;
        let named = r.stdout.trim().to_string();
        if r.code == Some(0) && !named.is_empty() && args::revision(&named).is_ok() {
            return Some(named);
        }
        ["main", "master", "trunk", "develop"].iter().find(|b| self.git(repo, &["rev-parse", "-q", "--verify", &format!("refs/heads/{b}")], false, 4, 15, cancel).is_ok_and(|r| r.code == Some(0))).map(|b| b.to_string())
    }

    /// `git.pr_draft`.
    pub(super) fn pr_draft(&self, repo: &Repo, a: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
        self.need_review()?;
        let head_arg = arg_str(a, "head");
        let head_rev = if head_arg.is_empty() { "HEAD" } else { head_arg };
        let (branch, uncommitted) = if head_arg.is_empty() {
            let (head, changed) = self.head_and_changes(repo, cancel);
            (head.branch().unwrap_or("").to_string(), changed)
        } else {
            (head_arg.to_string(), None)
        };
        let base = match arg_str(a, "base") {
            "" => self.default_base(repo, cancel).ok_or_else(|| bad("no main, master, trunk or develop branch and no origin/HEAD: say which branch the pull request is against with base="))?,
            b => b.to_string(),
        };
        let range = format!("{base}..{head_rev}");
        let count = self.ok(repo, &["rev-list", "--count", &range], false, 4, 30, cancel)?;
        let ahead: usize = count.stdout.trim().parse().unwrap_or(0);
        let log = self.ok(repo, &["log", "--no-color", "--no-merges", "-n", "100", "--pretty=format:%h%x09%s", &range], false, 64, 30, cancel)?;
        let commits = prdraft::parse_log(&log.stdout);
        let numstat = self.ok(repo, &["diff", "--numstat", "--no-color", "--no-ext-diff", "--no-textconv", &format!("{base}...{head_rev}")], false, 256, 60, cancel)?;
        let files = prdraft::parse_numstat(&numstat.stdout);
        let draft = prdraft::compose(&branch, &base, &commits, ahead, &files, uncommitted);
        let head_shown = if head_arg.is_empty() { if branch.is_empty() { "HEAD".to_string() } else { branch.clone() } } else { head_arg.to_string() };
        let text = format!(
            "Title: {}\n\n{}\n(This is text on this computer. Nothing was sent. forge.pr_create takes a title and a body of its own, with head={head_shown} and base={base}; it asks every time.)",
            if draft.title.is_empty() { "(none: the branch has no commit of its own)" } else { &draft.title },
            draft.body
        );
        Ok(ProviderOutput { data: json!({"repo": repo.entry.alias, "base": base, "head": head_shown, "title": draft.title, "body": draft.body, "commits": ahead, "files": files.len()}), text, ..Default::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn what_to_do_is_one_side_for_the_whole_file_or_a_list_of_hunks_and_never_both() {
        assert_eq!(parse_choices(&json!({"take": "ours"})).unwrap(), Choices::Every(Take::Ours));
        assert_eq!(parse_choices(&json!({"take": "both"})).unwrap(), Choices::Every(Take::Both));
        let list = parse_choices(&json!({"resolutions": [{"hunk": 2, "take": "theirs"}, {"hunk": 1, "take": "text", "text": "x\ny"}]})).unwrap();
        assert_eq!(list, Choices::Hunks(vec![(2, Take::Theirs), (1, Take::Text("x\ny".into()))]));
        for bad in [
            json!({}),
            json!({"take": "text"}),
            json!({"take": "mine"}),
            json!({"take": "ours", "resolutions": [{"hunk": 1, "take": "ours"}]}),
            json!({"resolutions": []}),
            json!({"resolutions": "all"}),
            json!({"resolutions": [{"take": "ours"}]}),
            json!({"resolutions": [{"hunk": 0, "take": "ours"}]}),
            json!({"resolutions": [{"hunk": 1, "take": "text"}]}),
            json!({"resolutions": [{"hunk": 1, "take": "ours", "text": "x"}]}),
            json!({"resolutions": [{"hunk": 1, "take": "other"}]}),
            json!({"resolutions": [{"hunk": 1, "take": "text", "text": "a\u{0}b"}]}),
        ] {
            assert!(parse_choices(&bad).is_err(), "{bad}");
        }
        let too_many: Vec<Value> = (1..=51).map(|n| json!({"hunk": n, "take": "ours"})).collect();
        assert!(parse_choices(&json!({"resolutions": too_many})).is_err());
        let long = "x".repeat(20_000);
        assert!(parse_choices(&json!({"resolutions": [{"hunk": 1, "take": "text", "text": long}, {"hunk": 2, "take": "text", "text": long}, {"hunk": 3, "take": "text", "text": long}, {"hunk": 4, "take": "text", "text": long}]})).is_err(), "the texts of a call are at most 60000 characters");
    }

    #[test]
    fn a_hash_and_a_plain_text_are_what_the_preview_and_the_call_agree_on() {
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(plain("\u{feff}a\r\nb\r\n"), "a\nb\n");
    }
}

//! `fs.edit_lines` (card OA6-02, task ED-07 of the OpenCode audit): the tool itself, and the preview of what it would do.
//!
//! It goes through the same door as `fs.edit` and does what `fs.edit` does around the change, step by step: the guard resolves the path for writing (a protected name, a file outside the
//! allowed folders and a link out of them are refused there), one writer holds the file, a folder, a file that is too big, a binary file and a file that is not UTF-8 are refused, a
//! read-only file is left alone, `expect_version` and what the question showed are compared with the file as it is read, the syntax gate looks at the result, the file is kept in the
//! undo store before it is replaced (reason `edited`, as for `fs.edit`), and the new content is put in place only while the file still is what was read. What differs is how the place is
//! found: by line numbers held to an anchor ([`super::super::lines`]), and not by the text to replace.

use super::super::lines::{self, FailKind, LineEdit, LineFail};
use super::*;

/// The call's arguments as a line edit. A number that is missing or is not a whole number is an error that says so.
pub(super) fn line_edit_of(args: &Value) -> Result<LineEdit, ToolError> {
    let number = |key: &str, what: &str| -> Result<usize, ToolError> {
        args.get(key)
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArguments, format!("{key} must be a whole number: {what}")))
    };
    let text = |key: &str| args.get(key).and_then(Value::as_str).map(String::from);
    Ok(LineEdit {
        start: number("start", "the first line to replace, counted from 1")?,
        end: number("end", "the last line to replace (start - 1 inserts before start)")?,
        new: arg_str(args, "new").to_string(),
        expect_first_line: text("expect_first_line"),
        expect_prev_line: text("expect_prev_line"),
    })
}

/// A refusal of the text work as a tool error: a stale anchor is a conflict the model can repeat, the rest is a mistake in the arguments. A refusal that quotes lines of the file is marked, as a
/// miss of `fs.edit` is, so that the host fences it and counts the run as having read the file (and a file of a trusted folder is text of the project: it does not end a run's edit grant).
fn failure(f: LineFail, of_the_project: bool) -> ToolError {
    let code = match f.kind {
        FailKind::Invalid => ErrorCode::InvalidArguments,
        FailKind::Stale => ErrorCode::Conflict,
    };
    let e = ToolError::new(code, f.message);
    match (f.quotes_file, of_the_project) {
        (true, true) => e.holding_outside_text().holding_local_text(),
        (true, false) => e.holding_outside_text(),
        (false, _) => e,
    }
}

/// The words for what an edit did to the lines, for the answer.
fn what_it_did(e: &LineEdit, applied: &lines::AppliedLines) -> String {
    let plural = |n: usize| if n == 1 { "line" } else { "lines" };
    let range = if e.start == e.end { format!("line {}", e.start) } else { format!("lines {}-{}", e.start, e.end) };
    if applied.removed == 0 {
        format!("Inserted {} {} before line {}", applied.added, plural(applied.added), e.start)
    } else if applied.added == 0 {
        format!("Took out {range}")
    } else {
        format!("Replaced {range} with {} {}", applied.added, plural(applied.added))
    }
}

pub(super) fn edit_lines(g: &PathGuard, trash: &Trash, args: &Value) -> Result<ProviderOutput, ToolError> {
    let r = g.resolve(arg_str(args, "path"), Op::Write)?;
    // One writer at a time for this file: a second edit reads what the first wrote (card OA6-01, ED-06).
    let _held = locks::lock(&[r.path.as_path()])?;
    let meta = std::fs::metadata(&r.path).map_err(|e| io_err(e, &display(&r.path)))?;
    if !meta.is_file() {
        return Err(ToolError::new(ErrorCode::InvalidArguments, format!("{} is not a file", display(&r.path))));
    }
    if meta.len() > g.limits().edit_max_file_bytes {
        return Err(ToolError::new(ErrorCode::ResourceLimit, "the file is too large to edit; use fs.write with the full content"));
    }
    let line_edit = line_edit_of(args)?;
    if let Some(v) = args.get("expect_version").and_then(|v| v.as_str()).filter(|v| !v.is_empty()) {
        let now = sha256_file(&r.path).map_err(|e| io_err(e, "hash"))?;
        if now != v {
            return Err(ToolError::new(ErrorCode::Conflict, "ERROR [FILE_CHANGED] the file changed since that version; read it again"));
        }
    }
    refuse_read_only(&r.path)?;
    let bytes = read_checked(&r)?;
    if looks_binary(&bytes[..bytes.len().min(8192)]) {
        return Err(ToolError::new(ErrorCode::InvalidArguments, "binary files cannot be edited"));
    }
    let before = sha256_hex(&bytes);
    // The file the person was shown is the file this call changes (card OA6-04, SE-13).
    check_shown(&r.path, &before)?;
    let text = String::from_utf8(bytes).map_err(|_| ToolError::new(ErrorCode::InvalidArguments, "the file is not valid UTF-8"))?;
    let mut doc = super::super::patch::Doc::parse(&text);
    let before_text = doc.text().to_string();
    // The anchor is looked at first and nothing is written unless it matches. A file that looks like it holds secrets is never quoted in a refusal.
    let applied = lines::apply_line_edit(&mut doc, &line_edit, !r.sensitive).map_err(|f| failure(f, r.trusted && !r.sensitive))?;
    let updated = doc.render();
    // The syntax gate (card OA6-03), as for `fs.edit`: an edit that breaks a JSON file that was valid is not made, and one that unbalances brackets or quotes is made and noted.
    let syntax_note = match syntax_verdict(&r.path, &before_text, doc.text()) {
        super::super::ast::Verdict::Refuse(why) => return Err(syntax_refusal(&r.path, &why)),
        super::super::ast::Verdict::Note(note) => Some(note),
        super::super::ast::Verdict::Fine => None,
    };
    // The diff of what the edit does, as the model may read it: the lines it wrote, and counts (card OA6-04).
    let name = display(&r.path);
    let mine = diff::Supplied::of([line_edit.new.as_str()]);
    let dry = arg_bool(args, "dry_run");
    let changed = diff::diff_with(&name, &name, &before_text, doc.text(), PREVIEW_CONTEXT, if dry { PREVIEW_LINES } else { RESULT_DIFF_LINES }, &mine);
    let delta = applied.added as i64 - applied.removed as i64;
    let place = json!([applied.first_line, applied.last_line]);
    let sentence = what_it_did(&line_edit, &applied);
    if dry {
        return out(
            json!({
                "dry_run": true, "path": name, "replacements": 1, "replaced": [line_edit.start, line_edit.end], "lines": place, "line_delta": delta, "total_lines": applied.total,
                "eol": doc.eol_label(), "bom": doc.bom(), "version": before, "changes": [change_entry(&name, "update", r.sensitive, Some(&changed))],
            }),
            format!(
                "Dry run: the edit applies cleanly to {name}: {} (+{} -{} lines, the file would have {} lines). Nothing was written.\n{}{}",
                sentence.to_lowercase(),
                changed.added,
                changed.removed,
                applied.total,
                if r.sensitive { "(the lines are not shown: this file looks like it holds secrets)" } else { changed.model_text.as_str() },
                syntax_note.as_deref().map(|n| format!("\nNote: {n}")).unwrap_or_default()
            ),
        );
    }
    let undo = trash.backup(&r.path, "edited").map_err(|e| ToolError::new(ErrorCode::BackendError, e))?;
    if let Err(e) = write_atomic_expecting(&r.path, updated.as_bytes(), Expect::Unchanged(&before)) {
        if is_changed_underneath(&e) {
            trash.drop_entry(&undo.undo_id);
        }
        return Err(write_err(e, &r.path, "write"));
    }
    let version = sha256_hex(updated.as_bytes());
    trash.record_after(&undo, &version);
    // What the model reads of the lines it changed: its own lines, and not the lines around them. Nothing of a file that looks like it holds secrets.
    let snippet = if r.sensitive || applied.added == 0 { String::new() } else { format!("\n{}", doc.own_snippet(applied.first_line, applied.last_line, &mine)) };
    let mut data = json!({
        "path": name, "replacements": 1, "replaced": [line_edit.start, line_edit.end], "lines": place, "line_delta": delta, "total_lines": applied.total,
        "eol": doc.eol_label(), "bom": doc.bom(), "undo_id": undo.undo_id, "version": version, "changes": [change_entry(&name, "update", r.sensitive, Some(&changed))],
    });
    let mut text = format!("{sentence} of {name}; the file has {} lines now ({delta:+}). The previous version is undo entry {}.{snippet}", applied.total, undo.undo_id);
    if let Some(note) = &syntax_note {
        data["syntax_note"] = json!(note);
        text.push_str(&format!("\nNote: {note}"));
    }
    out(data, text)
}

/// What `fs.edit_lines` would change, as a diff of the file as it is now (card OA6-04): the same as the question about `fs.edit` shows. Worked out with the code the edit itself runs, in memory;
/// any error (an anchor that does not match, say) gives no preview, and the call reports it itself.
pub(super) fn preview_edit_lines(g: &PathGuard, args: &Value) -> Option<ChangePreview> {
    let r = g.resolve(arg_str(args, "path"), Op::Write).ok()?;
    if r.sensitive {
        return Some(ChangePreview::new(aqua_tool_host::PREVIEW_HIDDEN));
    }
    let (sha, text) = read_for_preview(g, &r)?;
    if refuse_read_only(&r.path).is_err() || args.get("expect_version").and_then(|v| v.as_str()).is_some_and(|v| !v.is_empty() && v != sha) {
        return None;
    }
    let edit = line_edit_of(args).ok()?;
    let mut doc = super::super::patch::Doc::parse(&text);
    let before = doc.text().to_string();
    lines::apply_line_edit(&mut doc, &edit, false).ok()?;
    let name = display(&r.path);
    let d = diff::diff(&name, &name, &before, doc.text(), PREVIEW_CONTEXT, PREVIEW_LINES);
    let body = if d.is_empty() { format!("No change: {name} stays as it is.") } else { shown_diff(&d, &r) };
    Some(guarded(body, ShownFiles::one(&name, &sha)))
}

/// The sentence of the question about an edit by lines (the start of it is the one of `fs.edit`: `Edit <file>: ...`).
pub(super) fn question_summary(target: &str, args: &Value) -> String {
    let (start, end) = (arg_u64(args, "start", 0), arg_u64(args, "end", 0));
    let what = if end + 1 == start { format!("insert before line {start}") } else if start == end { format!("replace line {start}") } else { format!("replace lines {start}-{end}") };
    format!("Edit {target}: {what}{}", if arg_bool(args, "dry_run") { " (dry run: nothing is written)" } else { "" })
}

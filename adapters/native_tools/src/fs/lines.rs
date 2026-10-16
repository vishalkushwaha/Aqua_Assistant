//! `fs.edit_lines`: replace a range of lines of a file, held to an anchor (card OA6-02; task ED-07 of the OpenCode audit). Text in, text out: nothing here touches the disk.
//!
//! The edit says which lines it changes by number (`start` to `end`, counted from 1) and **proves it looked at them** by quoting the text of the line at `start` (`expect_first_line`) and,
//! optionally, of the line before it (`expect_prev_line`). A model that works from a view of the file that is out of date (an earlier edit moved the lines, somebody saved the file) has the
//! wrong numbers and cannot have the right text for them, so the edit is refused, nothing is written, and the answer quotes the lines the file has now so that the call can be repeated.
//!
//! * `end == start - 1` inserts before line `start`; `start == lines + 1` adds after the last line; an empty `new` takes the lines out.
//! * The file's line endings are kept (the same [`Doc`] as `fs.edit` uses): a CRLF file stays CRLF, a byte order mark stays, a file with no line break at its end has none after the edit, and
//!   a file that had one still has it. A replacement never glues to the line after it.
//! * An anchor is compared without the spaces and tabs at its ends (and without a line number and tab the model copied from `fs.read`), as the tolerant matcher of `fs.edit` forgives them.
//!   A blank line is no anchor alone: when line `start` is blank the line before it must be given too.

use super::patch::Doc;

/// One call of `fs.edit_lines`, as the model wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineEdit {
    /// The first line to replace, counted from 1.
    pub start: usize,
    /// The last line to replace; `start - 1` makes the edit an insertion before `start`.
    pub end: usize,
    /// What stands in place of the lines. Read as the text of whole lines: its last line break is optional.
    pub new: String,
    /// The text line `start` has, as the model saw it (required when the file has a line `start`).
    pub expect_first_line: Option<String>,
    /// The text of line `start - 1`, as the model saw it (optional; required when line `start` is blank).
    pub expect_prev_line: Option<String>,
}

/// What kind of mistake an edit was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailKind {
    /// The arguments cannot be what the model meant: a range that is backwards or past the end, a missing anchor, nothing to change.
    Invalid,
    /// The anchor is not what the file has: the model's view of the file is out of date.
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineFail {
    pub kind: FailKind,
    pub message: String,
    /// The message quotes lines of the file (so the host must fence it and count the run as having read the file).
    pub quotes_file: bool,
}

fn fail(kind: FailKind, message: impl Into<String>) -> LineFail {
    LineFail { kind, message: message.into(), quotes_file: false }
}

/// Where an edit landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedLines {
    /// The first and last line of the new text, counted in the file after the edit (both the line `start` for an edit that only takes lines out).
    pub first_line: usize,
    pub last_line: usize,
    /// How many lines were taken out and put in.
    pub removed: usize,
    pub added: usize,
    /// How many lines the file has after the edit.
    pub total: usize,
}

/// How many lines an LF text has: its line breaks, and one more when the last line has none.
pub fn line_count(text: &str) -> usize {
    text.bytes().filter(|b| *b == b'\n').count() + usize::from(text.as_bytes().last().is_some_and(|b| *b != b'\n'))
}

/// The byte offset at which each line starts (one entry for each line of [`line_count`]).
fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    if text.is_empty() {
        return starts;
    }
    starts.push(0);
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' && i + 1 < text.len() {
            starts.push(i + 1);
        }
    }
    starts
}

/// Line `n` (counted from 1) without its line break.
fn line_at<'a>(text: &'a str, starts: &[usize], n: usize) -> &'a str {
    let from = starts[n - 1];
    let to = starts.get(n).copied().unwrap_or(text.len());
    text[from..to].trim_end_matches('\n')
}

fn cut(s: &str) -> String {
    let shown: String = s.chars().take(120).collect();
    if s.chars().count() > 120 {
        format!("{shown}...")
    } else {
        shown
    }
}

/// `12<TAB>text` as `fs.read` prints a line: the text, when `s` starts with a line number and a tab.
fn after_number_and_tab(s: &str) -> Option<&str> {
    let t = s.trim_start();
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0).then(|| t[digits..].strip_prefix('\t')).flatten()
}

/// Is `want` the text of the line `have`? Spaces and tabs at the ends do not count, and a line number and tab copied from `fs.read` are taken off.
fn same_line(have: &str, want: &str) -> bool {
    let want = want.strip_prefix('\u{feff}').unwrap_or(want);
    if have.trim() == want.trim() {
        return true;
    }
    after_number_and_tab(want).is_some_and(|rest| have.trim() == rest.trim())
}

/// The lines around line `at` as `fs.read` prints them (`N<TAB>text`): two before and two after.
fn window(text: &str, starts: &[usize], at: usize) -> String {
    let total = starts.len();
    let from = at.saturating_sub(2).max(1);
    let to = (at + 2).min(total);
    (from..=to).map(|n| format!("{n}\t{}", cut(line_at(text, starts, n)))).collect::<Vec<_>>().join("\n")
}

/// Where else in the file a line like `want` is: up to three line numbers, the nearest to `at` first. A line that moved because of an earlier edit is found this way.
fn elsewhere(text: &str, starts: &[usize], want: &str, at: usize) -> String {
    if want.trim().is_empty() {
        return String::new();
    }
    let mut hits: Vec<usize> = (1..=starts.len()).filter(|n| *n != at && same_line(line_at(text, starts, *n), want)).collect();
    hits.sort_by_key(|n| n.abs_diff(at));
    hits.truncate(3);
    if hits.is_empty() {
        return String::new();
    }
    let list = hits.iter().map(usize::to_string).collect::<Vec<_>>().join(" and ");
    format!("A line like it is at line {list}. ")
}

/// The refusal of an anchor that is not what the file has. `quote`: the message may show the lines of the file (not for a file that looks like it holds secrets).
fn stale(text: &str, starts: &[usize], which: &str, line_no: usize, want: &str, quote: bool) -> LineFail {
    if !quote {
        return fail(FailKind::Stale, format!("the anchor does not match: {which} (line {line_no}) is not what you gave. Nothing was changed. (The lines are not shown: this file looks like it holds secrets.)"));
    }
    let where_else = elsewhere(text, starts, want, line_no);
    let message = format!(
        "the anchor does not match: line {line_no} is now \"{}\", not \"{}\". Nothing was changed. {where_else}The file may have changed since you read it, or the line number is wrong.\nthe lines now:\n{}\nRead the lines again, then repeat the call with start and end for those lines and the text of line start in expect_first_line.",
        cut(line_at(text, starts, line_no)),
        cut(want.trim()),
        window(text, starts, line_no)
    );
    LineFail { kind: FailKind::Stale, message, quotes_file: true }
}

/// The lines of `new` as whole lines, each with its line break: `""` is no line, `"a"` and `"a\n"` are one, `"\n"` is one blank line, `"a\n\n"` is two.
fn whole_lines(new: &str) -> Vec<&str> {
    if new.is_empty() {
        return Vec::new();
    }
    new.strip_suffix('\n').unwrap_or(new).split('\n').collect()
}

/// Makes one line edit in `doc`. Nothing is changed when it returns an error. `quote`: a refusal may show lines of the file.
pub fn apply_line_edit(doc: &mut Doc, e: &LineEdit, quote: bool) -> Result<AppliedLines, LineFail> {
    let text = doc.text().to_string();
    let starts = line_starts(&text);
    let total = starts.len();
    let (start, end) = (e.start, e.end);
    if start == 0 {
        return Err(fail(FailKind::Invalid, "start counts from 1: the first line of the file is line 1"));
    }
    if end + 1 < start {
        return Err(fail(FailKind::Invalid, format!("end ({end}) is before start ({start}): give end >= start to replace lines, or end = start - 1 to insert before line {start}")));
    }
    if start > total + 1 {
        return Err(fail(FailKind::Invalid, format!("the file has {total} line{}: start ({start}) is past the end; the last place to insert is line {}", if total == 1 { "" } else { "s" }, total + 1)));
    }
    if end > total {
        return Err(fail(FailKind::Invalid, format!("the file has {total} line{}: end ({end}) is past the end of the file", if total == 1 { "" } else { "s" })));
    }
    let insertion = end + 1 == start;
    let has_start_line = start <= total;

    // The anchor: the text of line `start`, and of the line before it.
    if has_start_line {
        let Some(want) = e.expect_first_line.as_deref() else {
            return Err(fail(FailKind::Invalid, format!("give expect_first_line: the text of line {start} as fs.read shows it (without the line number). The edit is made only if the line is what you saw, so that it cannot land on the wrong lines.")));
        };
        if !same_line(line_at(&text, &starts, start), want) {
            return Err(stale(&text, &starts, "expect_first_line", start, want, quote));
        }
        if line_at(&text, &starts, start).trim().is_empty() && start >= 2 && e.expect_prev_line.is_none() {
            return Err(fail(FailKind::Invalid, format!("line {start} is blank, and a blank line cannot anchor an edit alone: give expect_prev_line too (the text of line {})", start - 1)));
        }
    } else if total > 0 {
        // Adding after the last line: there is no line `start` to quote, so the last line is the anchor.
        if e.expect_prev_line.is_none() {
            return Err(fail(FailKind::Invalid, format!("to add after the last line (line {total}) give expect_prev_line: the text of line {total}")));
        }
        if let Some(want) = e.expect_first_line.as_deref().filter(|w| !w.trim().is_empty()) {
            return Err(fail(FailKind::Stale, format!("the anchor does not match: the file has {total} line{}, so there is no line {start} (expect_first_line says \"{}\"). Nothing was changed. The file may have changed since you read it: read it again.", if total == 1 { "" } else { "s" }, cut(want.trim()))));
        }
    }
    if let Some(want) = e.expect_prev_line.as_deref() {
        if start >= 2 {
            if !same_line(line_at(&text, &starts, start - 1), want) {
                return Err(stale(&text, &starts, "expect_prev_line", start - 1, want, quote));
            }
        } else if !want.trim().is_empty() {
            return Err(fail(FailKind::Stale, format!("the anchor does not match: there is no line before line 1, and expect_prev_line says \"{}\". Nothing was changed. Leave expect_prev_line out for line 1.", cut(want.trim()))));
        }
    }

    // The bytes the edit replaces, and what goes in. The break at the end of the last line is part of the lines it ends.
    let mut span_start = if has_start_line { starts[start - 1] } else { text.len() };
    let span_end = if insertion {
        span_start
    } else if end < total {
        starts[end]
    } else {
        text.len()
    };
    let new_text = e.new.replace("\r\n", "\n").replace('\r', "\n");
    let body: Vec<&str> = whole_lines(&new_text).into_iter().map(|l| l.strip_prefix('\u{feff}').unwrap_or(l)).collect();
    let mut repl: String = body.iter().map(|l| format!("{l}\n")).collect();
    // A file whose last line has no line break keeps it that way: the new last line gets none, and an added line follows a break the old last line was missing.
    let unterminated = !text.is_empty() && !text.ends_with('\n');
    if unterminated && span_end == text.len() {
        if insertion && span_start == text.len() {
            if !repl.is_empty() {
                repl = format!("\n{}", repl.strip_suffix('\n').unwrap_or(&repl));
            }
        } else if repl.is_empty() {
            // Taking out the last lines: the break of the line before them goes too, or the file would end with a break it did not have.
            if span_start > 0 {
                span_start -= 1;
            }
        } else {
            repl.pop();
        }
    }
    if text[span_start..span_end] == repl {
        return Err(fail(FailKind::Invalid, if insertion { "nothing to insert: give the lines to add in new".to_string() } else { format!("lines {start}-{end} already are what you gave; nothing to change. Give the lines that should differ.") }));
    }
    let removed = if insertion { 0 } else { end - start + 1 };
    doc.splice(span_start, span_end, &repl);
    let added = body.len();
    Ok(AppliedLines { first_line: start, last_line: if added > 0 { start + added - 1 } else { start }, removed, added, total: line_count(doc.text()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(start: usize, end: usize, new: &str, first: Option<&str>) -> LineEdit {
        LineEdit { start, end, new: new.into(), expect_first_line: first.map(String::from), expect_prev_line: None }
    }

    fn run(original: &str, e: &LineEdit) -> Result<String, LineFail> {
        let mut doc = Doc::parse(original);
        apply_line_edit(&mut doc, e, true).map(|_| doc.render())
    }

    #[test]
    fn a_range_is_replaced_when_the_anchor_is_the_line_at_start() {
        assert_eq!(run("a\nb\nc\nd\n", &edit(2, 3, "B\nC\n", Some("b"))).unwrap(), "a\nB\nC\nd\n");
        assert_eq!(run("a\nb\nc\nd\n", &edit(2, 2, "BB", Some("  b  "))).unwrap(), "a\nBB\nc\nd\n", "the new text needs no break of its own and the anchor no spaces at its ends");
        assert_eq!(run("a\nb\nc\nd\n", &edit(1, 4, "x\n", Some("a"))).unwrap(), "x\n");
        assert_eq!(run("a\nb\nc\nd\n", &edit(4, 4, "D1\nD2\n", Some("d"))).unwrap(), "a\nb\nc\nD1\nD2\n");
    }

    #[test]
    fn a_wrong_anchor_changes_nothing_and_shows_the_lines_the_file_has() {
        let mut doc = Doc::parse("one\ntwo\nthree\nfour\nfive\n");
        let err = apply_line_edit(&mut doc, &edit(3, 3, "X\n", Some("two")), true).unwrap_err();
        assert_eq!(err.kind, FailKind::Stale);
        assert!(err.quotes_file);
        assert!(err.message.contains("line 3 is now \"three\", not \"two\"") && err.message.contains("Nothing was changed"), "{}", err.message);
        assert!(err.message.contains("A line like it is at line 2."), "the line the model meant is found: {}", err.message);
        assert!(err.message.contains("1\tone\n2\ttwo\n3\tthree\n4\tfour\n5\tfive"), "{}", err.message);
        assert_eq!(doc.render(), "one\ntwo\nthree\nfour\nfive\n");
    }

    #[test]
    fn a_secret_file_is_refused_without_quoting_it() {
        let mut doc = Doc::parse("KEY=abc\nMODE=dev\n");
        let err = apply_line_edit(&mut doc, &edit(2, 2, "MODE=prod\n", Some("MODE=test")), false).unwrap_err();
        assert!(!err.quotes_file && !err.message.contains("MODE=dev") && !err.message.contains("KEY=abc") && err.message.contains("Nothing was changed"), "{}", err.message);
    }

    #[test]
    fn a_number_and_tab_copied_from_a_read_are_taken_off_the_anchor() {
        assert_eq!(run("a\nb\nc\n", &edit(2, 2, "B\n", Some("2\tb"))).unwrap(), "a\nB\nc\n");
        assert!(run("a\nb\nc\n", &edit(2, 2, "B\n", Some("3\tb"))).is_ok(), "the number is not looked at, the text is");
        assert!(run("a\nb\nc\n", &edit(2, 2, "B\n", Some("2\tc"))).is_err());
    }

    #[test]
    fn the_line_before_is_checked_when_it_is_given() {
        let mut e = edit(2, 2, "B\n", Some("b"));
        e.expect_prev_line = Some("a".into());
        assert_eq!(run("a\nb\nc\n", &e).unwrap(), "a\nB\nc\n");
        e.expect_prev_line = Some("zzz".into());
        let err = run("a\nb\nc\n", &e).unwrap_err();
        assert_eq!(err.kind, FailKind::Stale);
        assert!(err.message.contains("line 1 is now \"a\", not \"zzz\""), "{}", err.message);
        // Line 1 has no line before it: nothing, or a blank, is the only thing that fits.
        let mut first = edit(1, 1, "A\n", Some("a"));
        first.expect_prev_line = Some(String::new());
        assert_eq!(run("a\nb\n", &first).unwrap(), "A\nb\n");
        first.expect_prev_line = Some("x".into());
        assert_eq!(run("a\nb\n", &first).unwrap_err().kind, FailKind::Stale);
    }

    #[test]
    fn a_blank_line_needs_the_line_before_it_to_anchor() {
        let blank = edit(2, 2, "x\n", Some(""));
        let err = run("a\n\nc\n", &blank).unwrap_err();
        assert_eq!(err.kind, FailKind::Invalid);
        assert!(err.message.contains("expect_prev_line"), "{}", err.message);
        let mut both = blank.clone();
        both.expect_prev_line = Some("a".into());
        assert_eq!(run("a\n\nc\n", &both).unwrap(), "a\nx\nc\n");
        // The first line has none before it, so a blank first line anchors alone.
        assert_eq!(run("\nb\n", &edit(1, 1, "x\n", Some(""))).unwrap(), "x\nb\n");
    }

    #[test]
    fn the_anchor_is_required_unless_there_is_nothing_to_anchor() {
        let err = run("a\nb\n", &edit(1, 1, "x\n", None)).unwrap_err();
        assert_eq!(err.kind, FailKind::Invalid);
        assert!(err.message.contains("give expect_first_line"), "{}", err.message);
        // An empty file has no line to quote.
        assert_eq!(run("", &edit(1, 0, "first\n", None)).unwrap(), "first\n");
        // After the last line the last line is the anchor.
        assert!(run("a\nb\n", &edit(3, 2, "c\n", None)).unwrap_err().message.contains("expect_prev_line"));
        let mut tail = edit(3, 2, "c\n", None);
        tail.expect_prev_line = Some("b".into());
        assert_eq!(run("a\nb\n", &tail).unwrap(), "a\nb\nc\n");
        tail.expect_prev_line = Some("a".into());
        assert_eq!(run("a\nb\n", &tail).unwrap_err().kind, FailKind::Stale);
    }

    #[test]
    fn the_range_must_be_inside_the_file_and_the_right_way_round() {
        for (start, end, why) in [(0, 1, "counts from 1"), (3, 1, "before start"), (5, 5, "past the end"), (2, 4, "past the end")] {
            let err = run("a\nb\nc\n", &edit(start, end, "x\n", Some("b"))).unwrap_err();
            assert_eq!(err.kind, FailKind::Invalid, "{start}-{end}");
            assert!(err.message.contains(why), "{start}-{end}: {}", err.message);
        }
        // One line past the last is where a line is added; one more is not.
        let mut e = edit(4, 3, "d\n", None);
        e.expect_prev_line = Some("c".into());
        assert_eq!(run("a\nb\nc\n", &e).unwrap(), "a\nb\nc\nd\n");
        e.start = 5;
        e.end = 4;
        assert!(run("a\nb\nc\n", &e).unwrap_err().message.contains("past the end"));
    }

    #[test]
    fn an_empty_replacement_takes_the_lines_out_and_an_insertion_puts_lines_before_start() {
        assert_eq!(run("a\nb\nc\nd\n", &edit(2, 3, "", Some("b"))).unwrap(), "a\nd\n");
        assert_eq!(run("a\nb\nc\nd\n", &edit(1, 4, "", Some("a"))).unwrap(), "");
        assert_eq!(run("a\nb\nc\n", &edit(2, 1, "new1\nnew2\n", Some("b"))).unwrap(), "a\nnew1\nnew2\nb\nc\n");
        assert_eq!(run("a\nb\nc\n", &edit(1, 0, "top", Some("a"))).unwrap(), "top\na\nb\nc\n");
        let mut doc = Doc::parse("a\nb\nc\nd\n");
        let applied = apply_line_edit(&mut doc, &edit(2, 3, "x\n", Some("b")), true).unwrap();
        assert_eq!((applied.removed, applied.added, applied.first_line, applied.last_line, applied.total), (2, 1, 2, 2, 3));
    }

    #[test]
    fn nothing_to_change_is_refused() {
        assert!(run("a\nb\n", &edit(2, 2, "b\n", Some("b"))).unwrap_err().message.contains("already are what you gave"));
        assert!(run("a\nb\n", &edit(2, 1, "", Some("b"))).unwrap_err().message.contains("nothing to insert"));
    }

    #[test]
    fn crlf_stays_crlf_and_new_breaks_are_crlf_too() {
        assert_eq!(run("a\r\nb\r\nc\r\n", &edit(2, 2, "B1\nB2\n", Some("b"))).unwrap(), "a\r\nB1\r\nB2\r\nc\r\n");
        assert_eq!(run("a\r\nb\r\nc\r\n", &edit(2, 2, "B1\r\nB2\r\n", Some("b"))).unwrap(), "a\r\nB1\r\nB2\r\nc\r\n");
        assert_eq!(run("a\r\nb\r\nc\r\n", &edit(2, 3, "", Some("b"))).unwrap(), "a\r\n");
        assert_eq!(run("a\r\nb\r\n", &edit(3, 2, "c", None)).unwrap_err().kind, FailKind::Invalid, "the last line anchors an addition");
        let mut tail = edit(3, 2, "c\n", None);
        tail.expect_prev_line = Some("b".into());
        assert_eq!(run("a\r\nb\r\n", &tail).unwrap(), "a\r\nb\r\nc\r\n");
        // A line of a file with a mix keeps its own break.
        assert_eq!(run("a\nb\r\nc\n", &edit(1, 1, "A\n", Some("a"))).unwrap(), "A\nb\r\nc\n");
    }

    #[test]
    fn a_byte_order_mark_stays_and_the_anchor_of_line_one_is_the_text_after_it() {
        assert_eq!(run("\u{feff}a\nb\n", &edit(1, 1, "A\n", Some("a"))).unwrap(), "\u{feff}A\nb\n");
        assert_eq!(run("\u{feff}a\nb\n", &edit(1, 1, "A\n", Some("\u{feff}a"))).unwrap(), "\u{feff}A\nb\n");
        assert_eq!(run("\u{feff}a\nb\n", &edit(1, 1, "\u{feff}A\n", Some("a"))).unwrap(), "\u{feff}A\nb\n", "a mark the model copied into new is not written twice");
    }

    #[test]
    fn the_final_line_break_is_kept_or_kept_away() {
        // A file that ends with a break still does, whether or not the new text has one.
        assert_eq!(run("a\nb\n", &edit(2, 2, "B", Some("b"))).unwrap(), "a\nB\n");
        assert_eq!(run("a\nb\n", &edit(2, 2, "B\n", Some("b"))).unwrap(), "a\nB\n");
        // A file that does not end with one still does not.
        assert_eq!(run("a\nb", &edit(2, 2, "B\n", Some("b"))).unwrap(), "a\nB");
        assert_eq!(run("a\nb", &edit(2, 2, "B", Some("b"))).unwrap(), "a\nB");
        assert_eq!(run("a\nb", &edit(1, 1, "A\n", Some("a"))).unwrap(), "A\nb", "an edit away from the end leaves the end alone");
        assert_eq!(run("a\nb", &edit(2, 2, "", Some("b"))).unwrap(), "a", "taking out the last line takes its line before's break too");
        assert_eq!(run("a\nb", &edit(1, 2, "", Some("a"))).unwrap(), "");
        let mut tail = edit(3, 2, "c\n", None);
        tail.expect_prev_line = Some("b".into());
        assert_eq!(run("a\nb", &tail).unwrap(), "a\nb\nc", "a line added after a last line that had no break follows one");
        assert_eq!(run("only", &edit(1, 1, "ONE\nTWO\n", Some("only"))).unwrap(), "ONE\nTWO");
    }

    #[test]
    fn a_lone_cr_in_the_new_text_is_a_line_break_and_the_replacement_never_glues_to_the_next_line() {
        assert_eq!(run("a\nb\nc\n", &edit(2, 2, "x\ry", Some("b"))).unwrap(), "a\nx\ny\nc\n");
        assert_eq!(run("a\nb\nc\n", &edit(2, 2, "no break at the end", Some("b"))).unwrap(), "a\nno break at the end\nc\n");
        assert_eq!(run("a\nb\nc\n", &edit(2, 2, "\n", Some("b"))).unwrap(), "a\n\nc\n", "a break alone is one blank line");
        assert_eq!(run("a\nb\nc\n", &edit(2, 2, "x\n\n", Some("b"))).unwrap(), "a\nx\n\nc\n");
    }

    #[test]
    fn multibyte_text_is_cut_at_characters_and_a_long_line_is_cut_in_a_refusal() {
        assert_eq!(run("caf\u{e9}\n\u{2014} dash\n", &edit(2, 2, "\u{2014} dash!\n", Some("\u{2014} dash"))).unwrap(), "caf\u{e9}\n\u{2014} dash!\n");
        let long = "\u{e9}".repeat(300);
        let err = run(&format!("{long}\nb\n"), &edit(1, 1, "x\n", Some("zzz"))).unwrap_err();
        assert!(err.message.contains("...") && err.message.len() < 1500, "{}", err.message.len());
    }

    #[test]
    fn a_property_of_random_edits_the_lines_outside_the_range_are_never_touched() {
        // A small generator (no crate): files of 0 to 12 lines, in LF or CRLF, with and without a final break, and a random valid range with the right anchor.
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = |n: usize| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((seed >> 33) as usize) % n
        };
        let mut made = 0;
        for _ in 0..600 {
            let n = next(13);
            let lines: Vec<String> = (0..n).map(|i| format!("line {i} {}", "x".repeat(next(4)))).collect();
            let final_break = next(2) == 0;
            // A file with no line break, or one that is empty, has no line ending to keep: it is LF.
            let eol = if (n >= 2 || (n == 1 && final_break)) && next(2) == 0 { "\r\n" } else { "\n" };
            let mut original = lines.join(eol);
            if final_break && n > 0 {
                original.push_str(eol);
            }
            let start = 1 + next(n + 1);
            let end = if start > n { start - 1 } else { start - 1 + next(n - start + 2) };
            let k = next(3);
            let new_lines: Vec<String> = (0..k).map(|i| format!("new {i}")).collect();
            let new = new_lines.join("\n");
            let mut e = LineEdit { start, end, new, expect_first_line: lines.get(start - 1).cloned(), expect_prev_line: if start >= 2 { lines.get(start - 2).cloned() } else { None } };
            if start > n && n > 0 {
                e.expect_first_line = None;
            }
            let mut doc = Doc::parse(&original);
            let Ok(applied) = apply_line_edit(&mut doc, &e, true) else { continue };
            made += 1;
            let out = doc.render();
            // What the file should be: the lines before, the new lines, the lines after, in the file's own break, and its end as it was.
            let mut want: Vec<String> = lines[..start - 1].to_vec();
            want.extend(new_lines.iter().cloned());
            want.extend(lines[end.max(start - 1)..].iter().cloned());
            let mut expected = want.join(eol);
            let ends_with_break = if n == 0 { true } else { final_break };
            if ends_with_break && !want.is_empty() {
                expected.push_str(eol);
            }
            assert_eq!(out, expected, "file {original:?}, edit {e:?}");
            assert_eq!(applied.total, want.len(), "{e:?}");
            assert!(out.is_empty() || !out.contains("\r\n") || out.replace("\r\n", "").find(['\r', '\n']).is_none(), "no lone break in {out:?}");
        }
        assert!(made > 400, "the generator makes edits that apply: {made} of 600");
    }
}

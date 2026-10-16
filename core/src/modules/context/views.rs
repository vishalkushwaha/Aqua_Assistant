//! What the prompt shows of a result whose text has a shape worth keeping when it is cut (card OA5-03, CX-06).
//!
//! The prompt shows the newest result of a tool up to 900 tokens (2,700 characters) and the five before it up to 120 (360). A page of a file was cut like any other text: at a
//! character, from its start, so that the line that says where the next page begins (and which version the file is) was the part that was gone, and the last line shown was
//! half a line. [`fs_read_view`] cuts a page on a line, and writes the first line of the page again for the lines that are left, so that it still says which lines they are and from
//! where to go on.

use aqua_native_tools::fs::page::{Header, Status};

/// The note that a read of a file of secrets ends with, and the short form of it that is kept when there is not room for the whole.
const HIDDEN_NOTE_START: &str = "(The values in this file are hidden";
const HIDDEN_NOTE_SHORT: &str = "(values hidden; reveal=true shows them, asking the person)";

/// The most characters a line of a page is cut to when even one whole line does not fit.
const LEAST_LINE_CHARS: usize = 40;

/// `page`, the text of an `fs.read` (inside the fence that was put round it), cut to at most `cap` characters on a line boundary, or `None` when it is not a page (an error, a file
/// that is empty, any other text): the caller cuts it as it cuts any text.
///
/// A page that fits is returned as it is. A page that does not keeps its first line, written again for the lines that are kept (`lines 1-40 of 300 (continue with
/// offset_line=41); ...`: the numbers come from the text, since a call made earlier is not in the history that the prompt is built from), as many whole numbered lines as fit, and the
/// note of a file of secrets in its short form. At least one line is kept, cut at a character when it alone is too long, so that a page is never only the news that it was cut.
pub fn fs_read_view(page: &str, cap: usize) -> Option<String> {
    let mut lines = page.lines();
    let header = Header::parse(lines.next()?)?;
    if page.chars().count() <= cap {
        return Some(page.to_string());
    }
    // The numbered lines are the ones that follow the header in order; anything after them is a note of the tool's.
    let rest: Vec<&str> = lines.collect();
    let mut numbered = 0usize;
    while numbered < rest.len() && rest[numbered].starts_with(&format!("{}\t", header.first + numbered as u64)) {
        numbered += 1;
    }
    if numbered == 0 {
        return None;
    }
    let note = rest[numbered..].iter().any(|l| l.starts_with(HIDDEN_NOTE_START)).then_some(HIDDEN_NOTE_SHORT);
    let note_cost = note.map_or(0, |n| n.chars().count() + 1);
    // The header as it will be if k lines are kept; its length changes little with k, so it is worked out for the most.
    let head_for = |kept: usize| -> String {
        let last = header.first + kept as u64 - 1;
        Header { first: header.first, last, total: header.total, status: Status::Continue(last + 1), tail: header.tail.clone() }.render()
    };
    let budget = cap.saturating_sub(head_for(numbered).chars().count() + 1 + note_cost);
    let mut kept = 0usize;
    let mut used = 0usize;
    for line in &rest[..numbered] {
        let cost = line.chars().count() + 1;
        if used + cost > budget {
            break;
        }
        used += cost;
        kept += 1;
    }
    let mut out = String::new();
    if kept == 0 {
        // Not even the first line fits: a piece of it, and the way to go on from the line after it.
        let first = rest[0];
        let room = budget.max(LEAST_LINE_CHARS);
        let piece: String = first.chars().take(room.saturating_sub(14)).collect();
        out.push_str(&head_for(1));
        out.push('\n');
        out.push_str(&piece);
        out.push_str(" … [line cut]");
    } else {
        out.push_str(&head_for(kept));
        for line in &rest[..kept] {
            out.push('\n');
            out.push_str(line);
        }
    }
    if let Some(n) = note {
        out.push('\n');
        out.push_str(n);
    }
    Some(out)
}

/// The line that says that lines were left out of a result made of lines.
const MORE_LINES: &str = "… [more lines not shown]";

/// A result made of lines (a search, a listing) cut to `cap` characters on a line: its first line (how many it found, and where) is always kept, and a line that does not fit is left out
/// whole and not shown in part. A text that fits is returned as it is.
pub fn lines_view(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let room = cap.saturating_sub(MORE_LINES.chars().count() + 1);
    let mut out = String::new();
    let mut used = 0usize;
    for (i, line) in text.lines().enumerate() {
        let cost = line.chars().count() + 1;
        if i == 0 && cost > room {
            // The first line alone is too long: a piece of it.
            out.extend(line.chars().take(room));
            out.push('\n');
            break;
        }
        if used + cost > room {
            break;
        }
        out.push_str(line);
        out.push('\n');
        used += cost;
    }
    out.push_str(MORE_LINES);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_result_is_cut_on_a_line_and_keeps_its_first_line() {
        let text = format!("151 matches in 2 files\n{}", (1..=150).map(|i| format!("src/a.rs:{i}: let needle_{i} = {i};\n")).collect::<String>());
        let view = lines_view(&text, 700);
        assert!(view.chars().count() <= 700 && view.starts_with("151 matches in 2 files\nsrc/a.rs:1: let needle_1 = 1;\n") && view.ends_with(MORE_LINES), "{view}");
        // Every line that is shown is whole.
        for line in view.lines().skip(1).filter(|l| *l != MORE_LINES) {
            assert!(line.starts_with("src/a.rs:") && line.ends_with(';'), "{line}");
        }
        // A text that fits is as it is, and a first line that is longer than the room is cut at a character.
        assert_eq!(lines_view("two\nlines", 700), "two\nlines");
        let long = format!("{}\nsecond", "\u{e9}".repeat(900));
        let cut = lines_view(&long, 300);
        assert!(cut.chars().count() <= 300 && cut.starts_with("\u{e9}\u{e9}") && cut.ends_with(MORE_LINES) && !cut.contains("second"), "{}", cut.chars().count());
    }

    /// A page as `fs.read` writes it: the header, then `n` numbered lines of about thirty characters.
    fn page(first: u64, n: u64, total: u64, status: &str) -> String {
        let last = first + n - 1;
        let body: String = (first..=last).map(|i| format!("\n{i}\tlet value_{i} = compute({i});")).collect();
        format!("lines {first}-{last} of {total} ({status}); 9999 bytes; version {}{body}", "ab".repeat(32))
    }

    #[test]
    fn a_page_that_fits_is_left_as_it_is_and_what_is_not_a_page_is_not_touched() {
        let p = page(1, 20, 20, "whole file");
        assert_eq!(fs_read_view(&p, 5000), Some(p.clone()));
        assert_eq!(fs_read_view("ERROR [NOT_FOUND] fs.read: File not found: x.", 10), None);
        assert_eq!(fs_read_view("(empty file); 0 bytes; version abc", 10), None);
        assert_eq!(fs_read_view("a page of a web site with many words in it", 10), None);
        assert_eq!(fs_read_view("", 10), None);
        // A header with no numbered line after it is not a page either.
        assert_eq!(fs_read_view(&format!("lines 1-3 of 9 (continue with offset_line=4); 5 bytes; version {}\nnot numbered", "a".repeat(64)), 20), None);
    }

    /// Card OA5-03, CX-06: "a 310-line file read shows ~80 whole lines, then the hint with the right number".
    #[test]
    fn a_long_page_is_cut_on_a_line_and_its_first_line_is_written_again_for_the_lines_that_are_left() {
        let p = page(1, 110, 310, "continue with offset_line=111");
        assert!(p.chars().count() > 3000);
        let view = fs_read_view(&p, 2700).unwrap();
        assert!(view.chars().count() <= 2700, "{}", view.chars().count());
        let lines: Vec<&str> = view.lines().collect();
        let header = Header::parse(lines[0]).unwrap();
        // The header names the lines that are shown, the whole file's number of lines, and where to go on; the version is still there.
        let shown = lines.len() as u64 - 1;
        assert_eq!((header.first, header.last, header.total), (1, shown, Some(310)));
        assert_eq!(header.status, Status::Continue(shown + 1));
        assert!(lines[0].contains(&"ab".repeat(32)) && shown > 60 && shown < 110, "{shown} lines");
        // Every line shown is whole and in order, and the last one is the one before the line to go on from.
        for (i, l) in lines[1..].iter().enumerate() {
            assert!(l.starts_with(&format!("{}\tlet value_{}", i + 1, i + 1)) && l.ends_with(");"), "{l}");
        }
    }

    #[test]
    fn an_older_result_keeps_the_header_and_a_few_lines_and_the_way_to_go_on() {
        let p = page(41, 60, 400, "continue with offset_line=101");
        let view = fs_read_view(&p, 360).unwrap();
        assert!(view.chars().count() <= 360, "{}", view.chars().count());
        let first = view.lines().next().unwrap();
        let header = Header::parse(first).unwrap();
        assert_eq!((header.first, header.total), (41, Some(400)));
        assert!(matches!(header.status, Status::Continue(n) if n == header.last + 1) && header.last >= 41 && header.last < 50, "{first}");
        assert_eq!(view.lines().count() as u64, header.last - header.first + 2);
    }

    #[test]
    fn a_whole_file_that_is_cut_stops_being_whole_and_says_where_the_rest_begins() {
        let p = page(1, 100, 100, "whole file");
        let view = fs_read_view(&p, 1000).unwrap();
        let header = Header::parse(view.lines().next().unwrap()).unwrap();
        assert!(matches!(header.status, Status::Continue(n) if n == header.last + 1 && n < 100), "{header:?}");
        // A page that reaches the end of the file says the same once lines of it are left out.
        let end = fs_read_view(&page(201, 100, 300, "end of file"), 1000).unwrap();
        assert!(matches!(Header::parse(end.lines().next().unwrap()).unwrap().status, Status::Continue(_)));
    }

    #[test]
    fn one_line_that_alone_is_too_long_is_cut_at_a_character_and_the_read_goes_on_from_the_next() {
        let long = format!("lines 7-7 of 9 (continue with offset_line=8); 90000 bytes; version {}\n7\t{}", "c".repeat(64), "x".repeat(5000));
        let view = fs_read_view(&long, 400).unwrap();
        assert!(view.chars().count() <= 400, "{}", view.chars().count());
        assert!(view.ends_with(" … [line cut]") && view.lines().next().unwrap().starts_with("lines 7-7 of 9 (continue with offset_line=8)"), "{view}");
        // Multibyte text is cut at a character.
        let accents = format!("lines 1-1 of 3 (continue with offset_line=2); 9 bytes; version {}\n1\t{}", "d".repeat(64), "\u{e9}".repeat(3000));
        assert!(fs_read_view(&accents, 300).unwrap().chars().count() <= 300);
    }

    #[test]
    fn the_note_of_a_file_of_secrets_is_kept_in_its_short_form() {
        let mut p = page(1, 60, 60, "whole file");
        p.push_str("\n(The values in this file are hidden: the names and the length of each value are shown. To see the real values call fs.read again with reveal=true; the person is asked every time.)");
        let view = fs_read_view(&p, 700).unwrap();
        assert!(view.chars().count() <= 700 && view.ends_with(HIDDEN_NOTE_SHORT), "{view}");
        // A page of a file of secrets that fits keeps the whole note.
        let small = {
            let mut s = page(1, 3, 3, "whole file");
            s.push_str("\n(The values in this file are hidden: x)");
            s
        };
        assert_eq!(fs_read_view(&small, 5000), Some(small.clone()));
    }

    #[test]
    fn a_page_that_starts_late_in_the_file_is_numbered_from_where_it_starts() {
        // The numbers come from the text: the first numbered line after the header is the header's first line.
        let p = page(250, 60, 400, "continue with offset_line=310");
        let view = fs_read_view(&p, 800).unwrap();
        let mut lines = view.lines();
        let header = Header::parse(lines.next().unwrap()).unwrap();
        assert_eq!(header.first, 250);
        assert!(lines.next().unwrap().starts_with("250\t"));
    }
}

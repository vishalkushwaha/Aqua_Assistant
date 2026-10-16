//! Reading a file as pages that fit what the model is shown (card OA5-03: ED-01, CU-12, CU-03).
//!
//! `fs.read` used to read a whole line into memory whatever its length, looked for binary data in the first line only, reported "complete" for a page that the host then cut in the middle,
//! and put the way to go on, and the version of the file, at the end of the text, which is the part that is cut. The pieces here are what a page needs:
//!
//! * [`read_line_bounded`]: one line at a time, of which at most a cap is kept (the rest is skipped without being kept), so that a file of 200 MB with no line break costs no more memory
//!   than a short one;
//! * [`sniff`] and [`refused_by_extension`]: text or not, from the first 8 KiB and from the name;
//! * [`Header`]: the first line of a page, which says which lines it holds, how many the file has, how to go on and which version the file is; the loop's view of a result reads it back and
//!   writes it again when it shows fewer lines than the page held, so the line stays true;
//! * [`similar_names`] and [`fit_lines`]: names that look like the one asked for, and the lines of a result that fit a budget.

use std::io::BufRead;

/// The longest piece of one line that a read keeps and shows. The rest of a longer line is skipped, and the line says how many bytes it lost.
pub const MAX_LINE_BYTES: usize = 4096;
/// How many bytes at the start of a file decide whether it is text.
pub const SNIFF_BYTES: usize = 8192;
/// How far into a file a read looks for a line break before it says that the file is minified or binary (a file that is also over [`LONG_FILE_FACTOR`] times the read limit).
pub const NO_BREAK_PROBE_BYTES: usize = 65_536;
/// A file this many times larger than the read limit and with no line break in its first [`NO_BREAK_PROBE_BYTES`] is not read: one line of 16 MiB is not something to page through.
pub const LONG_FILE_FACTOR: u64 = 16;
/// The shortest and the longest page, in characters of numbered lines (`max_chars`).
pub const MIN_PAGE_CHARS: usize = 500;
pub const MAX_PAGE_CHARS: usize = aqua_tool_host::output::FS_READ_MAX_PAGE_CHARS;

/// What a line was like on disk, besides the part of it that was kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineInfo {
    /// The bytes of the line without its line break, kept or not.
    pub total: u64,
}

/// The next line of `reader` into `buf` (cleared first), at most `cap` bytes of it, without its line break. What is past `cap` is read and thrown away in chunks of what the reader has
/// buffered, so the memory used is `cap` and what the reader holds, whatever the length of the line. `None` at the end of the input. A last line with no line break is a line.
pub fn read_line_bounded<R: BufRead + ?Sized>(reader: &mut R, buf: &mut Vec<u8>, cap: usize) -> std::io::Result<Option<LineInfo>> {
    buf.clear();
    let mut total = 0u64;
    let mut any = false;
    loop {
        let chunk = match reader.fill_buf() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if chunk.is_empty() {
            return Ok(any.then_some(LineInfo { total }));
        }
        any = true;
        let (len, ends) = match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => (i, true),
            None => (chunk.len(), false),
        };
        let room = cap.saturating_sub(buf.len());
        buf.extend_from_slice(&chunk[..len.min(room)]);
        total += len as u64;
        reader.consume(if ends { len + 1 } else { len });
        if ends {
            return Ok(Some(LineInfo { total }));
        }
    }
}

/// The text of the start of a line that was cut at a byte count: a character that was cut in two is left out, bytes that are not UTF-8 become U+FFFD.
pub fn text_of_prefix(bytes: &[u8], cut: bool) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(e) if cut && e.error_len().is_none() => String::from_utf8_lossy(&bytes[..e.valid_up_to()]).into_owned(),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// What the start of a file says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sniff {
    Text,
    Binary,
    /// A byte order mark of UTF-16 (or UTF-32): text, but not the text `fs.read` reads.
    Utf16,
}

/// Is this the start of a text file? A UTF-16 byte order mark; a NUL byte in the first 8 KiB; or more than 30 % of those bytes that no text has (below 9, or 14 to 31: tab, line feed,
/// vertical tab, form feed and carriage return are text). The first line alone said nothing about a file whose first line was short and clean (card OA5-03, CU-12).
pub fn sniff(sample: &[u8]) -> Sniff {
    let sample = &sample[..sample.len().min(SNIFF_BYTES)];
    if sample.starts_with(&[0xFF, 0xFE]) || sample.starts_with(&[0xFE, 0xFF]) {
        return Sniff::Utf16;
    }
    if sample.contains(&0) {
        return Sniff::Binary;
    }
    let odd = sample.iter().filter(|b| **b < 9 || (13 < **b && **b < 32)).count();
    if odd * 100 > sample.len() * 30 {
        Sniff::Binary
    } else {
        Sniff::Text
    }
}

/// Is this the start of a file that is not text (for a search, which skips such a file, and for a read, which says so)?
pub fn looks_binary(sample: &[u8]) -> bool {
    sniff(sample) != Sniff::Text
}

/// Names of files that are not text, by their ending. A read says so before it opens one.
const BINARY_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "ico", "pdf", "zip", "gz", "7z", "exe", "dll", "so", "class", "jar", "wasm", "pyc", "docx", "xlsx", "pptx"];
/// Those of them that another tool reads.
const DOCUMENT_EXTENSIONS: &[&str] = &["pdf", "docx", "xlsx", "pptx"];

/// The refusal of a file that is not text by the ending of its name: what it is and, for a document, the tool that reads it. `None`: the name does not say.
pub fn refused_by_extension(shown: &str, file_name: &str) -> Option<String> {
    let ext = file_name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
    if !BINARY_EXTENSIONS.contains(&ext.as_str()) {
        return None;
    }
    Some(if DOCUMENT_EXTENSIONS.contains(&ext.as_str()) {
        format!("{shown} is a .{ext} document, not a text file; use doc.read for pdf, docx, xlsx and pptx files")
    } else {
        format!("{shown} looks like a binary file (.{ext}); fs.read only reads text")
    })
}

/// What a page says about its own place in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Lines 1 to the last of a file, and nothing in them was cut.
    Whole,
    /// The page reaches the end of the file.
    End,
    /// More lines follow: the next read starts at this line.
    Continue(u64),
}

/// The first line of the text of `fs.read`: `lines 1-62 of 300 (continue with offset_line=63); 18340 bytes; version <sha256>`. The count of lines is left out when it is not known (a file
/// over the size that is hashed), and `tail` is what follows the status, as it was written (`; 18340 bytes; version ...`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub first: u64,
    pub last: u64,
    pub total: Option<u64>,
    pub status: Status,
    pub tail: String,
}

impl Header {
    pub fn render(&self) -> String {
        let of = self.total.map(|t| format!(" of {t}")).unwrap_or_default();
        let status = match self.status {
            Status::Whole => "whole file".to_string(),
            Status::End => "end of file".to_string(),
            Status::Continue(n) => format!("continue with offset_line={n}"),
        };
        format!("lines {}-{}{of} ({status}){}", self.first, self.last, self.tail)
    }

    /// The header a line is, or `None` when it is anything else.
    pub fn parse(line: &str) -> Option<Header> {
        let rest = line.strip_prefix("lines ")?;
        let (range, rest) = rest.split_once(' ')?;
        let (first, last) = range.split_once('-')?;
        let (first, last) = (first.parse::<u64>().ok()?, last.parse::<u64>().ok()?);
        let (total, rest) = match rest.strip_prefix("of ") {
            Some(r) => {
                let (n, r) = r.split_once(' ')?;
                (Some(n.parse::<u64>().ok()?), r)
            }
            None => (None, rest),
        };
        let inner = rest.strip_prefix('(')?;
        let (status, tail) = inner.split_once(')')?;
        let status = match status {
            "whole file" => Status::Whole,
            "end of file" => Status::End,
            other => Status::Continue(other.strip_prefix("continue with offset_line=")?.parse::<u64>().ok()?),
        };
        Some(Header { first, last, total, status, tail: tail.to_string() })
    }
}

/// Up to `max` names from `siblings` that contain the stem of `asked` (the name without its ending), ignoring case, shortest first: what to offer when a file is not there. A stem of
/// fewer than two characters matches nothing (it would match everything).
pub fn similar_names(asked: &str, siblings: impl IntoIterator<Item = String>, max: usize) -> Vec<String> {
    let name = asked.rsplit(['/', '\\']).next().unwrap_or(asked).to_ascii_lowercase();
    let stem = match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => name.clone(),
    };
    if stem.chars().count() < 2 {
        return Vec::new();
    }
    let mut found: Vec<String> = siblings.into_iter().filter(|s| s.to_ascii_lowercase().contains(&stem)).collect();
    found.sort_by(|a, b| a.chars().count().cmp(&b.chars().count()).then_with(|| a.cmp(b)));
    found.truncate(max);
    found
}

/// How many of `lines` fit in `budget` characters (each counted with its line break). At least one is kept, so that a result is never only the news that it was cut.
pub fn fit_lines(lines: &[String], budget: usize) -> usize {
    let mut used = 0usize;
    for (i, l) in lines.iter().enumerate() {
        used += l.chars().count() + 1;
        if used > budget && i > 0 {
            return i;
        }
    }
    lines.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Read};

    fn lines_of(text: &[u8], cap: usize) -> Vec<(String, u64)> {
        let mut reader = BufReader::with_capacity(16, text);
        let mut buf = Vec::new();
        let mut out = Vec::new();
        while let Some(info) = read_line_bounded(&mut reader, &mut buf, cap).unwrap() {
            out.push((String::from_utf8_lossy(&buf).into_owned(), info.total));
        }
        out
    }

    #[test]
    fn lines_are_read_one_at_a_time_with_or_without_a_last_line_break() {
        assert_eq!(lines_of(b"a\nbb\nccc", 100), vec![("a".into(), 1), ("bb".into(), 2), ("ccc".into(), 3)]);
        assert_eq!(lines_of(b"a\n\nb\n", 100), vec![("a".into(), 1), ("".into(), 0), ("b".into(), 1)]);
        assert_eq!(lines_of(b"", 100), vec![]);
        assert_eq!(lines_of(b"\n", 100), vec![("".into(), 0)]);
        // A carriage return belongs to the line (the read takes it off when it shows it).
        assert_eq!(lines_of(b"a\r\nb\r\n", 100), vec![("a\r".into(), 2), ("b\r".into(), 2)]);
    }

    #[test]
    fn a_line_longer_than_the_cap_is_kept_up_to_the_cap_and_the_next_line_is_still_found() {
        let got = lines_of(b"0123456789\nshort\n", 4);
        assert_eq!(got, vec![("0123".into(), 10), ("shor".into(), 5)]);
        // The chunks of the reader are smaller than the line: what is skipped is skipped across chunks.
        let long = format!("{}\nend\n", "x".repeat(1000));
        assert_eq!(lines_of(long.as_bytes(), 8), vec![("xxxxxxxx".into(), 1000), ("end".into(), 3)]);
    }

    /// Card OA5-03, ED-01 acceptance: a file of 200 MB that is one line is walked through with the memory of the cap, not of the file.
    #[test]
    fn a_two_hundred_megabyte_line_takes_no_more_memory_than_the_cap() {
        let size: u64 = 200 << 20;
        let mut reader = BufReader::new(std::io::repeat(b'x').take(size));
        let mut buf = Vec::new();
        let info = read_line_bounded(&mut reader, &mut buf, MAX_LINE_BYTES).unwrap().unwrap();
        assert_eq!(info.total, size);
        assert_eq!(buf.len(), MAX_LINE_BYTES);
        assert!(buf.capacity() < 1 << 20, "the buffer grew to {} bytes", buf.capacity());
        assert!(read_line_bounded(&mut reader, &mut buf, MAX_LINE_BYTES).unwrap().is_none());
    }

    #[test]
    fn a_character_cut_in_two_is_left_out_and_bad_bytes_are_shown_as_one_mark() {
        let text = "ab\u{e9}\u{65e5}";
        let bytes = text.as_bytes();
        assert_eq!(text_of_prefix(&bytes[..3], true), "ab");
        assert_eq!(text_of_prefix(&bytes[..4], true), "ab\u{e9}");
        assert_eq!(text_of_prefix(&bytes[..5], true), "ab\u{e9}");
        assert_eq!(text_of_prefix(bytes, false), text);
        assert_eq!(text_of_prefix(b"a\xFFb", false), "a\u{fffd}b");
        assert_eq!(text_of_prefix(b"a\xFFb", true), "a\u{fffd}b", "bytes that are wrong in the middle are not a cut");
    }

    #[test]
    fn a_text_file_with_a_few_control_bytes_is_not_binary_but_a_blob_is() {
        // An escape sequence or two (a coloured log), a form feed, a tab and a carriage return are text.
        let log = format!("{}\x1b[31mred\x1b[0m\x0c\tdone\r\n", "plain line\n".repeat(50));
        assert_eq!(sniff(log.as_bytes()), Sniff::Text);
        assert_eq!(sniff(b""), Sniff::Text);
        // A NUL anywhere in the first 8 KiB, not only in the first line.
        let late = format!("{}\0{}", "clean line\n".repeat(100), "more\n".repeat(10));
        assert_eq!(sniff(late.as_bytes()), Sniff::Binary);
        // Past the first 8 KiB nothing is looked at.
        let mut far = vec![b'a'; SNIFF_BYTES];
        far.push(0);
        assert_eq!(sniff(&far), Sniff::Text);
        // More than 30 % of bytes no text has, and no NUL: a blob.
        let blob: Vec<u8> = (0..1000).map(|i| if i % 2 == 0 { 0x01 } else { b'a' }).collect();
        assert_eq!(sniff(&blob), Sniff::Binary);
        let some: Vec<u8> = (0..1000).map(|i| if i % 10 == 0 { 0x01 } else { b'a' }).collect();
        assert_eq!(sniff(&some), Sniff::Text, "one byte in ten is not 30 %");
        // PNG and PDF headers.
        assert!(looks_binary(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"));
        // UTF-16 with its mark, in both orders.
        assert_eq!(sniff(&[0xFF, 0xFE, b'a', 0]), Sniff::Utf16);
        assert_eq!(sniff(&[0xFE, 0xFF, 0, b'a']), Sniff::Utf16);
        // UTF-8 text with its mark and with accents is text.
        assert_eq!(sniff("\u{feff}caf\u{e9} \u{65e5}\u{672c}\n".as_bytes()), Sniff::Text);
    }

    #[test]
    fn a_name_that_says_binary_is_refused_with_the_tool_that_reads_it() {
        let pdf = refused_by_extension("@ws/a.PDF", "a.PDF").unwrap();
        assert!(pdf.contains("doc.read") && pdf.contains(".pdf"), "{pdf}");
        let png = refused_by_extension("@ws/a.png", "a.png").unwrap();
        assert!(png.contains("binary") && !png.contains("doc.read"), "{png}");
        assert!(refused_by_extension("@ws/a.rs", "a.rs").is_none() && refused_by_extension("@ws/png", "png").is_none() && refused_by_extension("@ws/Makefile", "Makefile").is_none());
    }

    #[test]
    fn a_header_is_written_and_read_back_in_every_form() {
        for h in [
            Header { first: 1, last: 62, total: Some(300), status: Status::Continue(63), tail: "; 18340 bytes; version abc".into() },
            Header { first: 1, last: 84, total: Some(84), status: Status::Whole, tail: "; 2341 bytes; version abc".into() },
            Header { first: 63, last: 300, total: Some(300), status: Status::End, tail: "; 18340 bytes; version abc".into() },
            Header { first: 5, last: 9, total: None, status: Status::Continue(10), tail: "; 80000000 bytes; too large to version".into() },
        ] {
            assert_eq!(Header::parse(&h.render()), Some(h.clone()), "{}", h.render());
        }
        assert_eq!(Header::parse("lines 1-62 of 300 (continue with offset_line=63); 18340 bytes; version abc").unwrap().tail, "; 18340 bytes; version abc");
        for not in ["", "(empty file)", "lines", "lines 1-x of 3 (whole file)", "lines 1-2 of 3 (soon)", "lines 1-2 of 3", "a page of lines 1-2 of 3 (whole file)", "1\tlines 1-2 of 3 (whole file)"] {
            assert_eq!(Header::parse(not), None, "{not:?}");
        }
    }

    #[test]
    fn names_that_look_like_the_one_asked_for_are_offered_shortest_first_and_only_a_few() {
        let names = || ["Notes.txt", "notes_old.txt", "readme.md", "my-notes-2024.txt", "NOTES.bak", "a.txt"].map(String::from);
        assert_eq!(similar_names("notes.md", names(), 3), vec!["NOTES.bak".to_string(), "Notes.txt".to_string(), "notes_old.txt".to_string()]);
        assert_eq!(similar_names("@ws/docs\\NOTES.MD", names(), 10).len(), 4, "the folder part of what was asked for is not the stem");
        assert!(similar_names("zzz.txt", names(), 3).is_empty());
        // A stem of one character would match everything.
        assert!(similar_names("a.txt", names(), 3).is_empty());
        assert!(similar_names("", names(), 3).is_empty());
        // A name with no ending is its own stem; a dot file keeps its dot.
        assert_eq!(similar_names("readme", names(), 3), vec!["readme.md".to_string()]);
    }

    #[test]
    fn lines_that_fit_a_budget_leave_the_rest_and_keep_at_least_one() {
        let lines: Vec<String> = ["aaaa", "bbbb", "cccc"].iter().map(|s| s.to_string()).collect();
        assert_eq!(fit_lines(&lines, 100), 3);
        assert_eq!(fit_lines(&lines, 10), 2, "each counts with its line break: 5 + 5 fits, 15 does not");
        assert_eq!(fit_lines(&lines, 3), 1, "the first is kept when it alone is over");
        assert_eq!(fit_lines(&[], 3), 0);
    }
}

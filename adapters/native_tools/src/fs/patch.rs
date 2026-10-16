//! Changing text by edits or by a unified diff: the part of `fs.edit` and `fs.patch` that is only text in, text out.
//!
//! * [`Doc`]: a file's text as an edit sees it. A leading byte order mark is set aside and every line break is read as `\n`, but the break each line really has (LF or CRLF) is kept, so
//!   that what is written back differs from what was there only where the edit was (card OA6-01, ED-02).
//! * [`apply_edit`], [`apply_edits`]: `{old, new}` pairs applied one after the other to one file's text. Each `old` must occur exactly once unless the edit says how many times
//!   (the same rule as `fs.edit`). When the exact text is missing, [`locate_unique`] tries a ladder of tolerant matches; every rung must find exactly one place, and no rung may
//!   widen the edit to a much larger span (ED-03).
//! * [`parse_unified`] and [`apply_hunks`]: a unified diff, as `git diff` writes it or a model writes it. The counts in a hunk header are not trusted (a model counts badly): the body
//!   is read until the next header, and a header that disagrees with its body is a warning. Declared line numbers are only a hint: a hunk is found by its context, the nearest match to
//!   where it says it should be, and a hunk whose context is not in the file is an error, never a guess; a hunk whose context is in several places far from where it says is refused
//!   with the places named (ED-04). Line endings (LF or CRLF) are kept as the file has them.
//!
//! Nothing here touches the disk; the provider reads the files, applies every change in memory, and writes only if all of them applied.

use super::diff::Supplied;
use std::collections::HashSet;
use std::sync::OnceLock;

/// One `{old, new}` edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub old: String,
    pub new: String,
    pub replace_all: bool,
    pub expect_count: Option<usize>,
}

/// What an edit says when `old` and `new` are the same text.
pub const IDENTICAL: &str = "old and new are identical; nothing to change. Give the lines that should differ.";

// ---- the text of a file, as an edit sees it ------------------------------------------------------------------------------------

/// The line ending a file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eol {
    Lf,
    Crlf,
}

/// What a file's text looks like apart from its characters: a byte order mark, and which line ending most of its lines have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextShape {
    pub bom: bool,
    /// The ending most lines have (CRLF when more than half of the line breaks are CRLF).
    pub eol: Eol,
    /// Some lines end in CRLF and some in LF.
    pub mixed: bool,
}

/// A file's text with the byte order mark set aside and every line break read as `\n`; the break each line really has is kept (`crlf`, one flag per `\n`), so that [`Doc::render`]
/// writes back the endings the file had, and a line break an edit adds takes the ending of the line the edit starts on.
#[derive(Debug, Clone)]
pub struct Doc {
    text: String,
    /// For each `\n` of `text`, in order: did it come from a CRLF?
    crlf: Vec<bool>,
    bom: bool,
    fallback: Eol,
}

fn count_newlines(s: &str) -> usize {
    s.as_bytes().iter().filter(|b| **b == b'\n').count()
}

/// Text a model wrote, with its line endings read as LF: CRLF and a lone CR become `\n`.
pub fn lf(s: &str) -> String {
    if !s.contains('\r') {
        return s.to_string();
    }
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// The `old` and `new` of an edit as the document sees them: line endings read as LF, and a byte order mark the model copied from `fs.read` taken off (from `new` too, when `old` had one).
fn clean_pair(old: &str, new: &str) -> (String, String) {
    let had_bom = old.starts_with('\u{feff}');
    let o = lf(old.strip_prefix('\u{feff}').unwrap_or(old));
    let n = lf(if had_bom { new.strip_prefix('\u{feff}').unwrap_or(new) } else { new });
    (o, n)
}

impl Doc {
    pub fn parse(original: &str) -> Doc {
        let (bom, body) = match original.strip_prefix('\u{feff}') {
            Some(rest) => (true, rest),
            None => (false, original),
        };
        let mut text = String::with_capacity(body.len());
        let mut crlf = Vec::new();
        let mut rest = body;
        while let Some(i) = rest.find('\n') {
            let line = &rest[..i];
            match line.strip_suffix('\r') {
                Some(stripped) => {
                    text.push_str(stripped);
                    crlf.push(true);
                }
                None => {
                    text.push_str(line);
                    crlf.push(false);
                }
            }
            text.push('\n');
            rest = &rest[i + 1..];
        }
        text.push_str(rest);
        let with_cr = crlf.iter().filter(|c| **c).count();
        let fallback = if with_cr * 2 > crlf.len() { Eol::Crlf } else { Eol::Lf };
        Doc { text, crlf, bom, fallback }
    }

    /// The text, line breaks as `\n`, no byte order mark.
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn bom(&self) -> bool {
        self.bom
    }

    pub fn shape(&self) -> TextShape {
        let with_cr = self.crlf.iter().filter(|c| **c).count();
        TextShape { bom: self.bom, eol: self.fallback, mixed: with_cr > 0 && with_cr < self.crlf.len() }
    }

    /// `"lf"`, `"crlf"` or `"mixed"` (a file with no line break is `"lf"`).
    pub fn eol_label(&self) -> &'static str {
        let s = self.shape();
        if s.mixed {
            "mixed"
        } else if self.crlf.first().copied().unwrap_or(false) {
            "crlf"
        } else {
            "lf"
        }
    }

    /// The text to write: the byte order mark if there was one, and each line break as it was (or as the edit that added it gave it).
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(self.text.len() + self.crlf.len() + 3);
        if self.bom {
            out.push('\u{feff}');
        }
        let mut flags = self.crlf.iter();
        let mut rest = self.text.as_str();
        while let Some(i) = rest.find('\n') {
            out.push_str(&rest[..i]);
            if flags.next().copied().unwrap_or(self.fallback == Eol::Crlf) {
                out.push('\r');
            }
            out.push('\n');
            rest = &rest[i + 1..];
        }
        out.push_str(rest);
        out
    }

    /// The 1-based line the byte `offset` of the text is on.
    pub fn line_of(&self, offset: usize) -> usize {
        count_newlines(&self.text[..offset.min(self.text.len())]) + 1
    }

    /// Replaces `text[start..end]` with `new` (LF text). A line break the replacement adds takes the ending of the line the span starts on; the breaks the span held keep theirs, in place.
    pub fn splice(&mut self, start: usize, end: usize, new: &str) {
        let before = count_newlines(&self.text[..start]);
        let held = count_newlines(&self.text[start..end]);
        let added = count_newlines(new);
        let start_line = self.crlf.get(before).copied().unwrap_or(self.fallback == Eol::Crlf);
        let mut mine: Vec<bool> = self.crlf[before..before + held].to_vec();
        mine.truncate(added);
        while mine.len() < added {
            mine.push(start_line);
        }
        self.crlf.splice(before..before + held, mine);
        self.text.replace_range(start..end, new);
    }

    /// Makes the replacement `hit` stands for and says where it landed.
    pub fn apply(&mut self, hit: &Located) -> Applied {
        self.splice(hit.start, hit.end, &hit.new);
        let first_line = self.line_of(hit.start);
        let breaks = count_newlines(&hit.new);
        let last_line = if hit.new.is_empty() { first_line } else { first_line + breaks - usize::from(hit.new.ends_with('\n') && breaks > 0) };
        Applied { kind: hit.kind, first_line, last_line, count: 1 }
    }

    /// A few numbered lines around lines `first..=last`, in the format `fs.read` uses (`N<TAB>text`): at most twelve, two before and two after the change. These are lines of the file, so
    /// no result a model reads is made of this (card OA6-04, follow-up of the security review): see [`Doc::own_snippet`].
    pub fn snippet(&self, first: usize, last: usize) -> String {
        let all: Vec<&str> = self.text.split('\n').collect();
        // The last "line" after a final break is not a line.
        let total = if self.text.ends_with('\n') { all.len() - 1 } else { all.len() };
        if total == 0 {
            return String::new();
        }
        let from = first.saturating_sub(2).max(1).min(total);
        let mut to = (last + 2).min(total).max(from);
        if to - from + 1 > 12 {
            to = from + 11;
        }
        (from..=to).map(|n| format!("{n}\t{}", all[n - 1].chars().take(120).collect::<String>())).collect::<Vec<_>>().join("\n")
    }

    /// The lines `first..=last` of the text as the model may read them after its own edit (card OA6-04, follow-up of the security review): at most twelve, none of the lines around them.
    /// A line the model supplied itself is printed as `fs.read` prints it (`N<TAB>text`); a line that holds text it did not supply (the rest of a line the edit changed only in part) is
    /// only counted, with its line number, so that no text of the file reaches the model through a tool whose results are not fenced.
    pub fn own_snippet(&self, first: usize, last: usize, own: &Supplied) -> String {
        let all: Vec<&str> = self.text.split('\n').collect();
        // The last "line" after a final break is not a line.
        let total = if self.text.ends_with('\n') { all.len() - 1 } else { all.len() };
        if total == 0 {
            return String::new();
        }
        let from = first.max(1).min(total);
        let to = last.min(total).max(from).min(from + 11);
        let mut out: Vec<String> = Vec::new();
        // The first and the number of the lines not shown, as a run.
        let mut hidden: Option<(usize, usize)> = None;
        let close = |hidden: &mut Option<(usize, usize)>, out: &mut Vec<String>| {
            if let Some((start, count)) = hidden.take() {
                let (lines, hold) = if count == 1 { ("line", "it holds") } else { ("lines", "they hold") };
                out.push(format!("{start}\t({count} {lines} not shown: {hold} text of the file)"));
            }
        };
        for n in from..=to {
            if own.holds(all[n - 1]) {
                close(&mut hidden, &mut out);
                out.push(format!("{n}\t{}", all[n - 1].chars().take(120).collect::<String>()));
            } else {
                hidden = Some(match hidden {
                    Some((start, count)) => (start, count + 1),
                    None => (n, 1),
                });
            }
        }
        close(&mut hidden, &mut out);
        out.join("\n")
    }
}

// ---- matching: exact first, then a ladder that must find exactly one place ----------------------------------------------------------

/// How the text of an edit was found in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    /// The text as given.
    Exact,
    /// Lines that are the same after trimming their ends.
    Trim,
    /// Lines that are the same once the indentation is ignored; the new text was moved to the file's indentation.
    Indent,
    /// The text had the `N<TAB>` line numbers `fs.read` prints in front of each line.
    LineNumbers,
    /// Curly quotes, long dashes, an ellipsis character or a no-break space stood where the file has plain ones.
    Unicode,
    /// The same words with different spaces between them.
    Whitespace,
    /// The text had `\n`, `\t` or `\"` written out as two characters.
    Escape,
}

impl MatchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MatchKind::Exact => "exact",
            MatchKind::Trim => "trim",
            MatchKind::Indent => "indent",
            MatchKind::LineNumbers => "line_numbers",
            MatchKind::Unicode => "unicode",
            MatchKind::Whitespace => "whitespace",
            MatchKind::Escape => "escape",
        }
    }

    /// The words that tell the model what was forgiven, so that it writes the next one right.
    pub fn how(self) -> &'static str {
        match self {
            MatchKind::Exact => "",
            MatchKind::Trim => "after ignoring whitespace at the ends of the lines",
            MatchKind::Indent => "after ignoring indentation (the new text was moved to the file's indentation)",
            MatchKind::LineNumbers => "after removing the line numbers at the start of each line (fs.read prints them; do not copy them)",
            MatchKind::Unicode => "after reading curly quotes, long dashes and no-break spaces as the plain characters the file has",
            MatchKind::Whitespace => "after ignoring differences in the spaces between words",
            MatchKind::Escape => "after reading \\n, \\t and \\\" as a line break, a tab and a quote",
        }
    }
}

/// One place in the (LF) text and what goes there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub start: usize,
    pub end: usize,
    pub kind: MatchKind,
    /// The replacement, adjusted for what the match forgave (indentation, escapes, a line break the span ended with).
    pub new: String,
}

/// Why no single place was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Miss {
    NotFound(String),
    Ambiguous(String),
    /// `old` has a line that stands for text left out (`...`).
    Elided(String),
}

impl Miss {
    pub fn message(&self) -> &str {
        match self {
            Miss::NotFound(m) | Miss::Ambiguous(m) | Miss::Elided(m) => m,
        }
    }
}

enum Rung {
    Miss,
    One(Located),
    Many(usize),
}

/// A span much larger than the text asked for is not the text asked for.
fn disproportionate(old: &str, span: &str) -> bool {
    span.chars().count() > old.chars().count().saturating_mul(3).saturating_add(80)
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn non_space_chars(s: &str) -> usize {
    s.chars().filter(|c| !c.is_whitespace()).count()
}

/// A tolerant rung that could pick a wrong place on a short text does not run for one (the exact and trimming rungs do).
const SHORT_TEXT: usize = 12;

/// The lines of `old`, without the break it ends with.
fn old_body_lines(old: &str) -> Vec<&str> {
    old.strip_suffix('\n').unwrap_or(old).split('\n').collect()
}

fn content(piece: &str) -> &str {
    piece.strip_suffix('\n').unwrap_or(piece)
}

/// The lines of the text with the byte offset each starts at; a piece keeps its break.
fn lines_of(text: &str) -> Vec<(usize, &str)> {
    let mut at = 0;
    text.split_inclusive('\n')
        .map(|piece| {
            let start = at;
            at += piece.len();
            (start, piece)
        })
        .collect()
}

fn leading_space(s: &str) -> &str {
    &s[..s.len() - s.trim_start().len()]
}

/// How the indentation of the lines that matched differs from the indentation of the lines that were asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Delta {
    Same,
    /// The file's lines have this much more in front.
    Add(String),
    /// The file's lines have this much less in front.
    Strip(String),
    Mixed,
}

fn indent_delta(window: &[(usize, &str)], asked: &[&str]) -> Delta {
    let mut found: Option<Delta> = None;
    for ((_, piece), want) in window.iter().zip(asked) {
        let have = content(piece);
        let (blank_have, blank_want) = (have.trim().is_empty(), want.trim().is_empty());
        if blank_have && blank_want {
            continue;
        }
        if blank_have || blank_want {
            return Delta::Mixed;
        }
        let (a, b) = (leading_space(have), leading_space(want));
        let d = if a == b {
            Delta::Same
        } else if let Some(p) = a.strip_suffix(b) {
            Delta::Add(p.to_string())
        } else if let Some(p) = b.strip_suffix(a) {
            Delta::Strip(p.to_string())
        } else {
            Delta::Mixed
        };
        found = Some(match found {
            None => d,
            Some(prev) if prev == d => prev,
            Some(_) => return Delta::Mixed,
        });
    }
    found.unwrap_or(Delta::Same)
}

fn reindent(new: &str, delta: &Delta) -> String {
    match delta {
        Delta::Add(p) => new.split('\n').map(|l| if l.trim().is_empty() { l.to_string() } else { format!("{p}{l}") }).collect::<Vec<_>>().join("\n"),
        Delta::Strip(p) => new.split('\n').map(|l| l.strip_prefix(p.as_str()).unwrap_or(l).to_string()).collect::<Vec<_>>().join("\n"),
        _ => new.to_string(),
    }
}

/// The places (line index, start, end) where whole lines of the text equal `want` once `norm` is applied to each; a span much larger than `old` is not counted.
fn window_hits(text: &str, lines: &[(usize, &str)], want: &[String], norm: &dyn Fn(&str) -> String, old: &str) -> Vec<(usize, usize, usize)> {
    let k = want.len();
    if k == 0 || lines.len() < k {
        return Vec::new();
    }
    let have: Vec<String> = lines.iter().map(|(_, p)| norm(content(p))).collect();
    let mut hits = Vec::new();
    for i in 0..=lines.len() - k {
        if have[i..i + k] == *want {
            let start = lines[i].0;
            let last = lines[i + k - 1];
            let end = last.0 + last.1.len();
            if !disproportionate(old, &text[start..end]) {
                hits.push((i, start, end));
            }
        }
    }
    hits
}

/// A whole-line match made into a replacement: the new text moves to the file's indentation when every line was indented the same way, and the line break the span ends with is kept
/// when the text asked for did not mention it.
fn finish_window(text: &str, lines: &[(usize, &str)], hit: (usize, usize, usize), asked: &[&str], old: &str, new: &str, kind: MatchKind) -> Located {
    let (i, start, end) = hit;
    let delta = indent_delta(&lines[i..i + asked.len()], asked);
    let mut new = reindent(new, &delta);
    let kind = if kind == MatchKind::Trim && delta != Delta::Same { MatchKind::Indent } else { kind };
    if text[start..end].ends_with('\n') && !old.ends_with('\n') && !new.ends_with('\n') {
        new.push('\n');
    }
    Located { start, end, kind, new }
}

/// A rung that compares whole lines after `norm`.
#[allow(clippy::too_many_arguments)]
fn window_rung(text: &str, lines: &[(usize, &str)], asked: &[&str], old: &str, new: &str, norm: &dyn Fn(&str) -> String, kind: MatchKind) -> Rung {
    let want: Vec<String> = asked.iter().map(|l| norm(l)).collect();
    let hits = window_hits(text, lines, &want, norm, old);
    match hits.len() {
        0 => Rung::Miss,
        1 => Rung::One(finish_window(text, lines, hits[0], asked, old, new, kind)),
        n => Rung::Many(n),
    }
}

fn read_prefix() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^\s*\d{1,7}(?:\t|: ?| \| ?)").expect("a fixed pattern"))
}

/// `s` with the `N<TAB>` that `fs.read` prints taken off every line, when every line that has text has one; `None` when some line has none (then it is not a copy of a read).
fn strip_read_prefix(s: &str) -> Option<String> {
    let re = read_prefix();
    let mut any = false;
    let mut out: Vec<&str> = Vec::new();
    for line in s.split('\n') {
        if line.trim().is_empty() {
            out.push(line);
            continue;
        }
        let m = re.find(line)?;
        any = true;
        out.push(&line[m.end()..]);
    }
    any.then(|| out.join("\n"))
}

fn fold_char(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => "'",
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => "\"",
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' => "-",
        '\u{2026}' => "...",
        '\u{00A0}' => " ",
        _ => return None,
    })
}

fn fold(s: &str) -> String {
    s.chars().map(|c| fold_char(c).map(str::to_string).unwrap_or_else(|| c.to_string())).collect()
}

/// `s` with the characters of [`fold_char`] replaced, and for each byte of the result the byte of `s` it came from (one more at the end, the length of `s`).
fn fold_with_map(s: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len() + 1);
    for (i, c) in s.char_indices() {
        match fold_char(c) {
            Some(r) => {
                out.push_str(r);
                map.extend(std::iter::repeat_n(i, r.len()));
            }
            None => {
                out.push(c);
                map.extend(std::iter::repeat_n(i, c.len_utf8()));
            }
        }
    }
    map.push(s.len());
    (out, map)
}

fn foldable(s: &str) -> bool {
    s.chars().any(|c| fold_char(c).is_some())
}

/// The text with `\n`, `\t`, `\r`, `\"` and `\\` read as the characters they stand for.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('n') => {
                chars.next();
                out.push('\n');
            }
            Some('t') => {
                chars.next();
                out.push('\t');
            }
            Some('r') => {
                chars.next();
            }
            Some('"') => {
                chars.next();
                out.push('"');
            }
            Some('\\') => {
                chars.next();
                out.push('\\');
            }
            _ => out.push('\\'),
        }
    }
    out
}

fn has_escape_literal(s: &str) -> bool {
    s.contains("\\n") || s.contains("\\t") || s.contains("\\\"")
}

/// A line that stands for text left out: `...`, `// ...`, `/* ... */`, or words like "rest of the file".
fn has_elision(old: &str) -> bool {
    old.lines().any(|l| {
        let t = l.trim();
        let bare = t.trim_start_matches(['/', '#', '*', '-', ' ']).trim_end_matches(['*', '/', ' ']);
        matches!(bare, "..." | "\u{2026}") || t.contains("rest of") || t.contains("unchanged")
    })
}

/// What a miss says before the lines of the file it quotes (the two hints below). They are text of the file, and a model reads them in the answer of a tool whose results are not fenced, so
/// the provider asks [`holds_file_text`] and has the host fence such an answer and count the run as having read the file (card OA6-04, follow-up of the security review).
const CLOSEST_LINES: &str = "the closest lines in the file are";
const NEARBY_LINES: &str = "nearby lines:";

/// Does this message of a failed edit quote lines of the file?
pub fn holds_file_text(message: &str) -> bool {
    message.contains(CLOSEST_LINES) || message.contains(NEARBY_LINES)
}

/// The closest place in the file to what was asked for, shown with line numbers and the first line that differs; `None` when nothing is close (at least half of the lines alike).
fn nearest_window(text: &str, old: &str) -> Option<String> {
    let want = old_body_lines(old);
    let lines: Vec<&str> = text.lines().collect();
    if want.len() > 40 || lines.is_empty() || lines.len() > 20_000 {
        return None;
    }
    let want_set: HashSet<&str> = want.iter().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
    if want_set.is_empty() {
        return None;
    }
    let k = want.len().min(lines.len());
    let mut best: Option<(f64, usize)> = None;
    for i in 0..=lines.len() - k {
        let window: HashSet<&str> = lines[i..i + k].iter().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
        let shared = want_set.intersection(&window).count();
        let score = shared as f64 / (want_set.len() + window.len() - shared).max(1) as f64;
        if best.is_none_or(|(b, _)| score > b) {
            best = Some((score, i));
        }
    }
    let (score, at) = best?;
    if score < 0.5 {
        return None;
    }
    let cut = |s: &str| s.chars().take(120).collect::<String>();
    let shown: Vec<String> = (at..at + k).take(6).map(|i| format!("{}\t{}", i + 1, cut(lines[i]))).collect();
    let differs = (0..k).find(|t| want[*t].trim() != lines[at + t].trim());
    let pair = differs.map(|t| format!("\nexpected: {}\nfile has: {}", cut(want[t].trim()), cut(lines[at + t].trim()))).unwrap_or_default();
    let more = if k > 6 { "\n..." } else { "" };
    let mut message = format!("the text to replace was not found; {CLOSEST_LINES} {}-{}:\n{}{more}{pair}\nCopy \"old\" from the file exactly, without line numbers.", at + 1, at + k, shown.join("\n"));
    if message.chars().count() > 600 {
        message = message.chars().take(600).collect();
    }
    Some(message)
}

/// A short excerpt so the next edit can copy a real line. At most three lines, each capped.
fn miss_message(text: &str, old: &str) -> String {
    let raw = old.lines().find(|l| !l.trim().is_empty()).unwrap_or(old).trim();
    let needle: String = raw.split_whitespace().take(2).collect::<Vec<_>>().join(" ").chars().take(80).collect();
    if needle.is_empty() {
        return "the text to replace was not found; read the file again and copy it exactly".into();
    }
    let mut near = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.contains(needle.as_str()) {
            let shown: String = line.chars().take(120).collect();
            near.push(format!("{}: {shown}", i + 1));
            if near.len() == 3 {
                break;
            }
        }
    }
    if near.is_empty() {
        "the text to replace was not found; read the file again and copy it exactly".into()
    } else {
        format!("the text to replace was not found; {NEARBY_LINES}\n{}", near.join("\n"))
    }
}

fn many(n: usize, how: &str) -> Miss {
    Miss::Ambiguous(format!("the text matches {n} places after {how}; add context to make it unique"))
}

/// What an edit says when `old` is in the file at two places that share text (`aa` in `aaa`): a search that goes on after each match does not see the second one, and the edit
/// would have been made at the first as if it were the only one (found by the property test of the edit engine, `tests/fs_properties.rs`).
const OVERLAPPING: &str = "the text occurs at two places that share some of its text (like aa in aaa); add context to make it unique, or set replace_all=true";

/// Is `old`, found at the byte `at` of `text`, also found at a place that starts inside it?
fn overlaps_itself(text: &str, old: &str, at: usize) -> bool {
    text[at..at + old.len()].char_indices().skip(1).any(|(i, _)| text[at + i..].starts_with(old))
}

/// Finds the one place in `text` (LF text) that `old` stands for, and what to put there.
///
/// The ladder, each rung only when the one before found nothing, each needing exactly one place (two are an error, never a pick), and each refusing a span much larger than `old`:
/// the exact text; lines equal after trimming (and the new text moved to the file's indentation); the `N<TAB>` prefixes of a copied read taken off; curly quotes and long dashes read as
/// plain ones; words equal whatever the spaces between them; `\n` and `\t` written out read as the characters. A text with an elision (`...`) is refused with a message that says so.
pub fn locate_unique(text: &str, old: &str, new: &str) -> Result<Located, Miss> {
    let found: Vec<usize> = text.match_indices(old).map(|(s, _)| s).collect();
    match found.len() {
        1 if overlaps_itself(text, old, found[0]) => return Err(Miss::Ambiguous(OVERLAPPING.into())),
        1 => return Ok(Located { start: found[0], end: found[0] + old.len(), kind: MatchKind::Exact, new: new.to_string() }),
        0 => {}
        n => return Err(Miss::Ambiguous(format!("the text occurs {n} times; add context to make it unique, or set replace_all=true"))),
    }
    let generic = || Miss::NotFound(nearest_window(text, old).unwrap_or_else(|| miss_message(text, old)));
    if old.trim().is_empty() {
        return Err(generic());
    }
    let lines = lines_of(text);
    let asked = old_body_lines(old);
    let trim = |s: &str| s.trim().to_string();

    // Lines equal after trimming; the new text follows the file's indentation.
    match window_rung(text, &lines, &asked, old, new, &trim, MatchKind::Trim) {
        Rung::One(hit) => return Ok(hit),
        Rung::Many(n) => return Err(many(n, "trimming")),
        Rung::Miss => {}
    }

    // The line numbers of a copied read.
    if let Some(bare) = strip_read_prefix(old) {
        let bare_new = strip_read_prefix(new).unwrap_or_else(|| new.to_string());
        let exact: Vec<usize> = text.match_indices(bare.as_str()).map(|(s, _)| s).collect();
        match exact.len() {
            1 => return Ok(Located { start: exact[0], end: exact[0] + bare.len(), kind: MatchKind::LineNumbers, new: bare_new }),
            0 => {
                let bare_lines = old_body_lines(&bare);
                match window_rung(text, &lines, &bare_lines, &bare, &bare_new, &trim, MatchKind::LineNumbers) {
                    Rung::One(hit) => return Ok(hit),
                    Rung::Many(n) => return Err(many(n, "removing the line numbers")),
                    Rung::Miss => {}
                }
            }
            n => return Err(many(n, "removing the line numbers")),
        }
    }

    let long_enough = non_space_chars(old) >= SHORT_TEXT;

    // Curly quotes, long dashes, an ellipsis character, no-break spaces.
    if long_enough && (foldable(old) || foldable(text)) {
        let (folded_text, map) = fold_with_map(text);
        let folded_old = fold(old);
        let at_edge = |i: usize| i == 0 || i >= map.len() - 1 || map[i] != map[i - 1];
        let spots: Vec<usize> = folded_text.match_indices(folded_old.as_str()).map(|(s, _)| s).filter(|s| at_edge(*s) && at_edge(s + folded_old.len())).collect();
        match spots.len() {
            1 => return Ok(Located { start: map[spots[0]], end: map[spots[0] + folded_old.len()], kind: MatchKind::Unicode, new: new.to_string() }),
            0 => {
                let fold_trim = |s: &str| fold(s).trim().to_string();
                match window_rung(text, &lines, &asked, old, new, &fold_trim, MatchKind::Unicode) {
                    Rung::One(hit) => return Ok(hit),
                    Rung::Many(n) => return Err(many(n, "reading curly quotes and long dashes as plain ones")),
                    Rung::Miss => {}
                }
            }
            n => return Err(many(n, "reading curly quotes and long dashes as plain ones")),
        }
    }

    // The same words with other spaces between them.
    let want = collapse(old);
    let k = asked.len();
    if lines.len() >= k {
        let mut hits = Vec::new();
        for i in 0..=lines.len() - k {
            let start = lines[i].0;
            let last = lines[i + k - 1];
            let end = last.0 + last.1.len();
            let span = &text[start..end];
            if collapse(span) == want && !disproportionate(old, span) {
                hits.push((i, start, end));
            }
        }
        match hits.len() {
            0 => {}
            1 => return Ok(finish_window(text, &lines, hits[0], &asked, old, new, MatchKind::Whitespace)),
            n => return Err(many(n, "ignoring the spaces between words")),
        }
    }

    // `\n` and `\t` written out as two characters (the file has none).
    if long_enough && !old.contains('\n') && has_escape_literal(old) && !has_escape_literal(text) {
        let plain = unescape(old);
        let plain_new = if !new.contains('\n') && has_escape_literal(new) { unescape(new) } else { new.to_string() };
        let exact: Vec<usize> = text.match_indices(plain.as_str()).map(|(s, _)| s).collect();
        match exact.len() {
            1 => return Ok(Located { start: exact[0], end: exact[0] + plain.len(), kind: MatchKind::Escape, new: plain_new }),
            0 => {
                let plain_lines = old_body_lines(&plain);
                match window_rung(text, &lines, &plain_lines, &plain, &plain_new, &trim, MatchKind::Escape) {
                    Rung::One(hit) => return Ok(hit),
                    Rung::Many(n) => return Err(many(n, "reading \\n and \\t as escapes")),
                    Rung::Miss => {}
                }
            }
            n => return Err(many(n, "reading \\n and \\t as escapes")),
        }
    }

    if has_elision(old) {
        return Err(Miss::Elided("old contains an elision (...). Give the full lines, or a shorter exact piece.".into()));
    }
    Err(generic())
}

/// Replaces `old` with `new` once. An exact match wins; else the tolerant ladder of [`locate_unique`], and only when that finds one place.
pub fn replace_unique(text: &str, old: &str, new: &str) -> Result<String, String> {
    let mut doc = Doc::parse(text);
    let (old, new) = clean_pair(old, new);
    match locate_unique(doc.text(), &old, &new) {
        Ok(hit) => {
            doc.apply(&hit);
            Ok(doc.render())
        }
        Err(miss) => Err(miss.message().to_string()),
    }
}

// ---- edits ---------------------------------------------------------------------------------------------------------------------

/// How an edit was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    pub kind: MatchKind,
    /// The first and the last line of the new text, counted in the file after the edit.
    pub first_line: usize,
    pub last_line: usize,
    /// How many places were changed.
    pub count: usize,
}

/// What kind of mistake an edit was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailKind {
    /// Nothing to do or nothing to go by (empty `old`, `old` and `new` the same, an elision).
    Invalid,
    NotFound,
    /// More places than the edit allows, or a count that is not the one expected.
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditFail {
    pub kind: FailKind,
    pub message: String,
}

fn fail(kind: FailKind, message: impl Into<String>) -> EditFail {
    EditFail { kind, message: message.into() }
}

/// Makes one edit in `doc`. Line endings of `old` and `new` are read as the file's; the edit may not be a no-op.
pub fn apply_edit(doc: &mut Doc, e: &Edit) -> Result<Applied, EditFail> {
    let (old, new) = clean_pair(&e.old, &e.new);
    if old.is_empty() {
        return Err(fail(FailKind::Invalid, "'old' is empty; give the exact text to replace"));
    }
    if old == new {
        return Err(fail(FailKind::Invalid, IDENTICAL));
    }
    let count = doc.text().matches(old.as_str()).count();
    if count == 0 && !e.replace_all && e.expect_count.is_none() {
        return match locate_unique(doc.text(), &old, &new) {
            Ok(hit) => Ok(doc.apply(&hit)),
            Err(Miss::Ambiguous(m)) => Err(fail(FailKind::Conflict, m)),
            Err(Miss::NotFound(m)) => Err(fail(FailKind::NotFound, m)),
            Err(Miss::Elided(m)) => Err(fail(FailKind::Invalid, m)),
        };
    }
    if count == 0 {
        return Err(fail(FailKind::NotFound, "the text to replace was not found; read the file again and copy it exactly"));
    }
    match (e.expect_count, e.replace_all) {
        (Some(want), _) if want != count => return Err(fail(FailKind::Conflict, format!("expected {want} occurrences but found {count}"))),
        (None, false) if count != 1 => return Err(fail(FailKind::Conflict, format!("the text occurs {count} times; add context to make it unique, or set replace_all=true"))),
        (None, false) if doc.text().find(old.as_str()).is_some_and(|at| overlaps_itself(doc.text(), &old, at)) => return Err(fail(FailKind::Conflict, OVERLAPPING)),
        _ => {}
    }
    // Every place, the last first, so that the offsets of the earlier ones stay right.
    let starts: Vec<usize> = doc.text().match_indices(old.as_str()).map(|(s, _)| s).collect();
    for s in starts.iter().rev() {
        doc.splice(*s, *s + old.len(), &new);
    }
    let first_line = doc.line_of(starts[0]);
    let breaks = count_newlines(&new);
    let last_line = if new.is_empty() { first_line } else { first_line + breaks - usize::from(new.ends_with('\n') && breaks > 0) };
    Ok(Applied { kind: MatchKind::Exact, first_line, last_line, count })
}

/// What a list of edits did to one file.
#[derive(Debug, Clone)]
pub struct EditsOutcome {
    pub text: String,
    pub applied: Vec<Applied>,
    /// `"lf"`, `"crlf"` or `"mixed"`, as the file was.
    pub eol: &'static str,
    pub bom: bool,
}

/// Applies `edits` in order to `text`. `label` names the file in error messages. Every failure says which edit it was and what to do.
pub fn apply_edits_detailed(text: &str, edits: &[Edit], label: &str) -> Result<EditsOutcome, String> {
    let mut doc = Doc::parse(text);
    let mut applied = Vec::with_capacity(edits.len());
    for (i, e) in edits.iter().enumerate() {
        match apply_edit(&mut doc, e) {
            Ok(a) => applied.push(a),
            Err(f) => return Err(format!("{label}, edit {}: {}", i + 1, f.message)),
        }
    }
    Ok(EditsOutcome { text: doc.render(), applied, eol: doc.eol_label(), bom: doc.bom() })
}

/// Applies `edits` in order to `text`; see [`apply_edits_detailed`].
pub fn apply_edits(text: &str, edits: &[Edit], label: &str) -> Result<String, String> {
    apply_edits_detailed(text, edits, label).map(|o| o.text)
}

// ---- a unified diff ------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Context(String),
    Remove(String),
    Add(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// Where the diff says the hunk starts in the old file (1-based; a hint only).
    pub old_start: usize,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// The path the diff names for the old and the new file (`a/` and `b/` prefixes as written; `/dev/null` for a created or deleted file).
    pub old_path: String,
    pub new_path: String,
    pub hunks: Vec<Hunk>,
    /// Things the diff got wrong that did not stop it from being applied (a header that counts other lines than the body has).
    pub warnings: Vec<String>,
}

impl FileDiff {
    /// The file this diff changes: the new path, or the old one when the file is deleted.
    pub fn target(&self) -> &str {
        if self.new_path == "/dev/null" {
            &self.old_path
        } else {
            &self.new_path
        }
    }

    pub fn creates_file(&self) -> bool {
        self.old_path == "/dev/null"
    }

    pub fn deletes_file(&self) -> bool {
        self.new_path == "/dev/null"
    }
}

fn header_path(rest: &str) -> String {
    // `--- a/src/lib.rs\t2024-01-01 ...` or `--- a/src/lib.rs`: the path ends at a tab.
    rest.split('\t').next().unwrap_or("").trim().to_string()
}

fn parse_range(spec: &str) -> Option<(usize, usize)> {
    let spec = spec.trim_start_matches(['-', '+']);
    let (start, len) = match spec.split_once(',') {
        Some((a, b)) => (a, b),
        None => (spec, "1"),
    };
    Some((start.parse().ok()?, len.parse().ok()?))
}

/// The files and hunks of a unified diff. A diff with no hunk, or a hunk that does not parse, is an error.
///
/// The numbers in a hunk header are not what decides where the hunk ends: its lines run until the next `@@` header, the next `---`/`+++` pair, a `diff --git` line or the end of the text.
/// A model that counts the lines of its own hunk wrong loses none of them. The header only tells a blank line that follows the last numbered line whether it was meant to be part of the hunk,
/// and a header that disagrees with the body is reported in [`FileDiff::warnings`].
pub fn parse_unified(diff: &str) -> Result<Vec<FileDiff>, String> {
    let lines: Vec<&str> = diff.lines().collect();
    let mut files: Vec<FileDiff> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        if let (Some(old), Some(new)) = (l.strip_prefix("--- "), lines.get(i + 1).and_then(|n| n.strip_prefix("+++ "))) {
            files.push(FileDiff { old_path: header_path(old), new_path: header_path(new), hunks: Vec::new(), warnings: Vec::new() });
            i += 2;
            continue;
        }
        if let Some(rest) = l.strip_prefix("@@ ") {
            let Some(file) = files.last_mut() else { return Err("a hunk (@@ ... @@) comes before any file header (--- / +++)".into()) };
            let spec = rest.split("@@").next().unwrap_or("");
            let mut parts = spec.split_whitespace();
            let (Some(old), Some(new)) = (parts.next().and_then(parse_range), parts.next().and_then(parse_range)) else {
                return Err(format!("the hunk header '{l}' is not in the form @@ -a,b +c,d @@"));
            };
            let mut body: Vec<Line> = Vec::new();
            let (mut old_seen, mut new_seen) = (0usize, 0usize);
            // Blank lines are either a context line whose space was stripped on the way or blank lines after the diff: they count once something else follows, or while the header still wants lines.
            let mut blanks = 0usize;
            i += 1;
            while i < lines.len() {
                let b = lines[i];
                if b.starts_with("@@ ") || b.starts_with("diff --git") || (b.starts_with("--- ") && lines.get(i + 1).is_some_and(|n| n.starts_with("+++ "))) {
                    break;
                }
                let counted = old_seen >= old.1 && new_seen >= new.1;
                let flush = |body: &mut Vec<Line>, blanks: &mut usize, old_seen: &mut usize, new_seen: &mut usize| {
                    for _ in 0..*blanks {
                        body.push(Line::Context(String::new()));
                        *old_seen += 1;
                        *new_seen += 1;
                    }
                    *blanks = 0;
                };
                match b.chars().next() {
                    Some('+') => {
                        flush(&mut body, &mut blanks, &mut old_seen, &mut new_seen);
                        body.push(Line::Add(b[1..].to_string()));
                        new_seen += 1;
                    }
                    Some('-') => {
                        flush(&mut body, &mut blanks, &mut old_seen, &mut new_seen);
                        body.push(Line::Remove(b[1..].to_string()));
                        old_seen += 1;
                    }
                    Some(' ') => {
                        flush(&mut body, &mut blanks, &mut old_seen, &mut new_seen);
                        body.push(Line::Context(b[1..].to_string()));
                        old_seen += 1;
                        new_seen += 1;
                    }
                    None => blanks += 1,
                    Some('\\') => {}
                    // Words after the last numbered line (a model explaining its diff) end the hunk; inside the numbered lines they are a mistake worth naming.
                    Some(_) if counted => break,
                    Some(_) => return Err(format!("line {} of the diff is not part of a hunk: '{}'", i + 1, b.chars().take(60).collect::<String>())),
                }
                i += 1;
            }
            // Blank lines at the end belong to the hunk only while its header wants more lines.
            while blanks > 0 && (old_seen < old.1 || new_seen < new.1) {
                body.push(Line::Context(String::new()));
                old_seen += 1;
                new_seen += 1;
                blanks -= 1;
            }
            // "\ No newline at end of file" after the last line of a hunk.
            while i < lines.len() && lines[i].starts_with('\\') {
                i += 1;
            }
            if body.is_empty() {
                return Err(format!("the hunk '{l}' has no lines"));
            }
            if (old_seen, new_seen) != (old.1, new.1) {
                let n = file.hunks.len() + 1;
                file.warnings.push(format!("hunk {n} of {}: the header says -{},{} +{},{} but the body has -{},{old_seen} +{},{new_seen}; applied as written", file.target(), old.0, old.1, new.0, new.1, old.0, new.0));
            }
            file.hunks.push(Hunk { old_start: old.0, lines: body });
            continue;
        }
        i += 1;
    }
    if files.is_empty() || files.iter().all(|f| f.hunks.is_empty()) {
        return Err("no change found: expected a unified diff with --- / +++ headers and @@ hunks".into());
    }
    if let Some(f) = files.iter().find(|f| f.hunks.is_empty()) {
        return Err(format!("the file {} has no hunks", f.target()));
    }
    Ok(files)
}

/// How far from where the diff says a hunk starts the search for its context goes.
const SEARCH_LINES: usize = 400;

/// A hunk whose context is in several places goes to the nearest one when that one is this close to the line the hunk names (a model is a few lines out); farther than that and the choice is not
/// the diff's to make.
const NEAR_ENOUGH: usize = 3;

/// How alike the lines of a hunk and the lines of the file must be to count as the same. The passes run in this order, and the last two (which forgive more) need exactly one place.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    Strict,
    /// Spaces at the end of a line do not count (a model drops them).
    TrailingSpace,
    /// Spaces at either end do not count: the hunk was written with other indentation than the file has.
    Trimmed,
    /// As `Trimmed`, and curly quotes, long dashes and no-break spaces read as the plain ones.
    Folded,
}

impl Pass {
    fn same(self, have: &str, want: &str) -> bool {
        match self {
            Pass::Strict => have == want,
            Pass::TrailingSpace => have.trim_end() == want.trim_end(),
            Pass::Trimmed => have.trim() == want.trim(),
            Pass::Folded => fold(have).trim() == fold(want).trim(),
        }
    }

    fn needs_one_place(self) -> bool {
        matches!(self, Pass::Trimmed | Pass::Folded)
    }

    fn how(self) -> &'static str {
        match self {
            Pass::Strict | Pass::TrailingSpace => "ignoring spaces at the ends of lines",
            Pass::Trimmed => "ignoring indentation",
            Pass::Folded => "reading curly quotes and long dashes as plain ones",
        }
    }
}

fn matches_at(file: &[String], at: usize, old: &[&str], pass: Pass) -> bool {
    old.iter().enumerate().all(|(k, want)| file.get(at + k).is_some_and(|have| pass.same(have.trim_end_matches('\r'), want)))
}

/// Applies the hunks of one file's diff to `text`.
///
/// The lines a hunk keeps (its context) are the file's own lines, not the diff's copy of them, so a hunk matched while ignoring spaces does not change the lines it said it leaves alone.
pub fn apply_hunks(text: &str, hunks: &[Hunk], label: &str) -> Result<String, String> {
    let (bom, text) = match text.strip_prefix('\u{feff}') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let crlf = text.matches("\r\n").count() * 2 > text.matches('\n').count();
    // Lines without their "\n"; a "\r" stays on the line so that a CRLF file is written back as it was.
    let mut file: Vec<String> = text.split('\n').map(str::to_string).collect();
    let mut shift: isize = 0;
    for (h, hunk) in hunks.iter().enumerate() {
        let n = h + 1;
        let old: Vec<&str> = hunk.lines.iter().filter_map(|l| if let Line::Context(s) | Line::Remove(s) = l { Some(s.as_str()) } else { None }).collect();
        // Where the diff says it goes, moved by what the earlier hunks of this file added or removed. A hunk with no old lines (only
        // additions) goes after line `old_start`.
        let expected = if old.is_empty() { hunk.old_start as isize + shift } else { hunk.old_start as isize - 1 + shift }.max(0) as usize;
        // The file has one more "line" after its last "\n"; an insertion cannot go past the lines it really has.
        let real_len = if file.last().is_some_and(|l| l.is_empty()) { file.len() - 1 } else { file.len() };
        let (at, pass) = if old.is_empty() {
            (expected.min(real_len), Pass::Strict)
        } else {
            let found = [Pass::Strict, Pass::TrailingSpace, Pass::Trimmed, Pass::Folded].iter().find_map(|pass| {
                let mut candidates: Vec<usize> = (0..=real_len.saturating_sub(old.len())).filter(|p| p.abs_diff(expected) <= SEARCH_LINES).collect();
                candidates.sort_by_key(|p| (p.abs_diff(expected), *p));
                let hits: Vec<usize> = candidates.into_iter().filter(|p| matches_at(&file, *p, &old, *pass)).collect();
                (!hits.is_empty()).then_some((*pass, hits))
            });
            let Some((pass, hits)) = found else {
                let first = old.first().map(|s| s.trim()).unwrap_or("");
                return Err(format!("{label}, hunk {n}: its lines are not in the file near line {} (looking for '{}'); read the file again and copy the lines exactly", hunk.old_start, first.chars().take(60).collect::<String>()));
            };
            if pass.needs_one_place() && hits.len() > 1 {
                return Err(format!("{label}, hunk {n}: its lines match {} places after {}; add context lines to make it unique", hits.len(), pass.how()));
            }
            if hits.len() > 1 && hits[0].abs_diff(expected) > NEAR_ENOUGH {
                let mut places: Vec<usize> = hits.iter().map(|p| p + 1).collect();
                places.sort_unstable();
                let shown: Vec<String> = places.iter().take(6).map(|p| p.to_string()).collect();
                let more = if places.len() > 6 { ", ..." } else { "" };
                return Err(format!("{label}, hunk {n} matches at lines [{}{more}] but the hunk says line {}; add context lines or correct the line numbers", shown.join(", "), hunk.old_start));
            }
            (hits[0], pass)
        };
        // Matched while ignoring indentation: the added lines follow the file's indentation when every line was indented the same way.
        let delta = if pass.needs_one_place() {
            let window: Vec<(usize, &str)> = (0..old.len()).map(|k| (0usize, file[at + k].as_str())).collect();
            indent_delta(&window, &old)
        } else {
            Delta::Same
        };
        let mut new: Vec<String> = Vec::new();
        let mut k = 0;
        for line in &hunk.lines {
            match line {
                Line::Context(_) => {
                    new.push(file[at + k].clone());
                    k += 1;
                }
                Line::Remove(_) => k += 1,
                Line::Add(s) => {
                    let s = reindent(s, &delta);
                    new.push(if crlf { format!("{s}\r") } else { s });
                }
            }
        }
        file.splice(at..at + old.len(), new.iter().cloned());
        shift += new.len() as isize - old.len() as isize;
    }
    let mut body = file.join("\n");
    // A file that does not end with a line break has no ending to give its last line. A line a hunk put there took the file's CR with it (the rule for the lines a hunk adds, above),
    // and a CR at the end of the file is not a line ending: the file used to come back with one it never had (found by the property test of a diff made from a change).
    if crlf && !text.ends_with(['\n', '\r']) && body.ends_with('\r') {
        body.pop();
    }
    Ok(if bom { format!("\u{feff}{body}") } else { body })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old: &str, new: &str) -> Edit {
        Edit { old: old.into(), new: new.into(), replace_all: false, expect_count: None }
    }

    #[test]
    fn a_unique_trimmed_span_replaces_once_and_two_spans_do_not() {
        let got = replace_unique("fn add() { \n}\n", "fn add() {\n}", "fn add() {\n    1\n}").unwrap();
        assert_eq!(got, "fn add() {\n    1\n}\n", "{got:?}");
        let missed = replace_unique("fn add() {}\nfn other() {}\n", "fn add MISSING", "fn add() {}").unwrap_err();
        assert!(missed.contains("not found") && missed.contains("1: fn add()"), "{missed}");
        assert!(replace_unique("a\na\n", "a", "b").is_err());
        assert_eq!(replace_unique("exact", "exact", "next").unwrap(), "next");
        let spaced = replace_unique("fn  add() {}\n", "fn add() {}", "fn add() { 1 }\n").unwrap();
        assert!(spaced.contains('1'), "{spaced:?}");
        assert!(replace_unique("fn  add() {}\nfn  add() {}\n", "fn add() {}", "x").is_err());
    }

    #[test]
    fn edits_apply_in_order_and_each_old_text_must_be_unique_unless_said_otherwise() {
        assert_eq!(apply_edits("a b c", &[edit("a", "x"), edit("x b", "y")], "f").unwrap(), "y c", "the second edit sees the first one's result");
        let e = apply_edits("a a b", &[edit("a", "x")], "f.txt").unwrap_err();
        assert!(e.contains("f.txt, edit 1") && e.contains("occurs 2 times"), "{e}");
        assert_eq!(apply_edits("a a b", &[Edit { replace_all: true, ..edit("a", "x") }], "f").unwrap(), "x x b");
        assert_eq!(apply_edits("a a b", &[Edit { expect_count: Some(2), ..edit("a", "x") }], "f").unwrap(), "x x b");
        assert!(apply_edits("a a b", &[Edit { expect_count: Some(3), ..edit("a", "x") }], "f").unwrap_err().contains("expected 3 occurrences but found 2"));
        assert!(apply_edits("a", &[edit("zzz", "x")], "f").unwrap_err().contains("not found"));
        assert!(apply_edits("a", &[edit("", "x")], "f").unwrap_err().contains("'old' is empty"));
        // Nothing is half-applied: an error in the second edit returns an error, and the caller keeps the original text.
        assert!(apply_edits("a b", &[edit("a", "x"), edit("q", "z")], "f").unwrap_err().contains("edit 2"));
    }

    // ---- ED-02: line endings, the byte order mark, the line break after a span, no-ops ---------------------------------------

    #[test]
    fn crlf_single_line_edit_keeps_crlf() {
        assert_eq!(apply_edits("a\r\nb\r\nc\r\n", &[edit("b", "B")], "f").unwrap(), "a\r\nB\r\nc\r\n");
        // The last line of a file that does not end with a break.
        assert_eq!(apply_edits("a\r\nb", &[edit("b", "B")], "f").unwrap(), "a\r\nB");
        // Deleting the text of a line leaves the line break the line had.
        assert_eq!(apply_edits("a\r\nb\r\nc\r\n", &[edit("b", "")], "f").unwrap(), "a\r\n\r\nc\r\n");
    }

    #[test]
    fn crlf_multiline_edit_inserts_crlf() {
        // `old` and `new` are written with LF, as `fs.read` shows the file; the file keeps CRLF in every line.
        assert_eq!(apply_edits("a\r\nb\r\nc\r\nd\r\n", &[edit("b\nc", "B\nC\nD")], "f").unwrap(), "a\r\nB\r\nC\r\nD\r\nd\r\n");
        // One line becomes three.
        assert_eq!(apply_edits("a\r\nb\r\nc\r\n", &[edit("b", "x\ny\nz")], "f").unwrap(), "a\r\nx\r\ny\r\nz\r\nc\r\n");
        // Three become one.
        assert_eq!(apply_edits("a\r\nb\r\nc\r\nd\r\n", &[edit("a\nb\nc", "abc")], "f").unwrap(), "abc\r\nd\r\n");
    }

    #[test]
    fn lf_file_crlf_old_matches() {
        // A model that wrote its text with CRLF still finds the lines of an LF file, and the file stays LF.
        assert_eq!(apply_edits("a\nb\nc\n", &[edit("a\r\nb", "x\r\ny")], "f").unwrap(), "x\ny\nc\n");
        assert_eq!(apply_edits("a\nb\nc\n", &[edit("b", "x\r\ny")], "f").unwrap(), "a\nx\ny\nc\n");
    }

    #[test]
    fn mixed_file_edits_with_the_eol_of_the_matched_line() {
        let text = "a\r\nb\nc\r\nd\n";
        // The line `b` ends in LF: the break the edit adds is LF, and the lines it did not touch keep theirs.
        assert_eq!(apply_edits(text, &[edit("b", "b1\nb2")], "f").unwrap(), "a\r\nb1\nb2\nc\r\nd\n");
        // The line `c` ends in CRLF.
        assert_eq!(apply_edits(text, &[edit("c", "c1\nc2")], "f").unwrap(), "a\r\nb\nc1\r\nc2\r\nd\n");
        // A span of two lines with the same number of lines keeps each line's own ending.
        assert_eq!(apply_edits(text, &[edit("a\nb", "A\nB")], "f").unwrap(), "A\r\nB\nc\r\nd\n");
        let doc = Doc::parse(text);
        assert_eq!(doc.eol_label(), "mixed");
        assert_eq!(doc.render(), text, "parsing and writing back changes nothing");
    }

    #[test]
    fn bom_first_line_edit() {
        let text = "\u{feff}fn main() {}\r\nfn other() {}\r\n";
        let got = apply_edits(text, &[edit("fn main() {}", "fn run() {}")], "f").unwrap();
        assert_eq!(got, "\u{feff}fn run() {}\r\nfn other() {}\r\n", "the mark and the endings stay");
        // The match that ignores the ends of the lines works on line 1 too (the mark is not part of the line).
        let indented = "\u{feff}  let a = 1;\nnext\n";
        let got = apply_edits(indented, &[edit("let a = 1; ", "let a = 2;")], "f").unwrap();
        assert!(got.starts_with('\u{feff}') && got.contains("let a = 2;") && !got.contains("let a = 1;"), "{got:?}");
        // A mark the model copied from the read is taken off.
        let got = apply_edits(text, &[edit("\u{feff}fn main() {}", "\u{feff}fn run() {}")], "f").unwrap();
        assert_eq!(got, "\u{feff}fn run() {}\r\nfn other() {}\r\n");
        // A file without one does not get one.
        assert_eq!(apply_edits("a\n", &[edit("a", "b")], "f").unwrap(), "b\n");
    }

    #[test]
    fn fuzzy_span_keeps_the_following_newline() {
        // Whole lines are replaced (the match ignores the indentation); the break after the last of them stays, so the next line is not glued on.
        let got = apply_edits("foo();\n  bar();\nbaz();\n", &[edit("foo();\nbar();", "foo(1);\nbar(1);")], "f").unwrap();
        assert_eq!(got, "foo(1);\nbar(1);\nbaz();\n");
        // The same with the spaces between words not the same.
        let got = apply_edits("fn  a() {}\nnext();\n", &[edit("fn a() {}", "fn a() { 1 }")], "f").unwrap();
        assert_eq!(got, "fn a() { 1 }\nnext();\n");
        // A CRLF file.
        let got = apply_edits("fn  a() {}\r\nnext();\r\n", &[edit("fn a() {}", "fn a() { 1 }")], "f").unwrap();
        assert_eq!(got, "fn a() { 1 }\r\nnext();\r\n");
        // `old` that names the break itself replaces it, and a `new` without one removes the line.
        let got = apply_edits("  a();\nb();\n", &[edit("a(); \n", "")], "f").unwrap();
        assert_eq!(got, "b();\n");
        // The last line of a file with no break after it gets none (and the new text follows the file's indentation).
        let got = apply_edits("x\n  tail();", &[edit("tail(); ", "end();")], "f").unwrap();
        assert_eq!(got, "x\n  end();");
    }

    #[test]
    fn identical_old_new_is_refused() {
        for (old, new) in [("a", "a"), ("a\nb", "a\r\nb"), ("a\r\nb", "a\nb")] {
            let e = apply_edits("a\nb\n", &[edit(old, new)], "f.txt").unwrap_err();
            assert!(e.contains("f.txt, edit 1") && e.contains("identical"), "{old:?} {new:?}: {e}");
        }
        // The edit that follows is not tried, and nothing is returned half done.
        assert!(apply_edits("a b", &[edit("a", "x"), edit("b", "b")], "f").unwrap_err().contains("edit 2"));
    }

    #[test]
    fn replace_all_on_crlf() {
        assert_eq!(apply_edits("x\r\nx\r\nx\r\n", &[Edit { replace_all: true, ..edit("x", "y") }], "f").unwrap(), "y\r\ny\r\ny\r\n");
        assert_eq!(apply_edits("x\r\nx\r\nz\r\n", &[Edit { replace_all: true, ..edit("x", "a\nb") }], "f").unwrap(), "a\r\nb\r\na\r\nb\r\nz\r\n");
        assert_eq!(apply_edits("k=1\r\nm\r\nk=1\r\n", &[Edit { expect_count: Some(2), ..edit("k=1", "k=2") }], "f").unwrap(), "k=2\r\nm\r\nk=2\r\n");
    }

    /// A small deterministic generator (the tests need no `rand`).
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, n: usize) -> usize {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % n.max(1)
        }
    }

    #[test]
    fn after_any_edit_of_a_crlf_file_no_line_break_is_alone() {
        let mut rng = Lcg(11);
        for round in 0..300 {
            let n = 3 + rng.next(12);
            let lines: Vec<String> = (0..n).map(|k| format!("line {round} {k} {}", rng.next(5))).collect();
            let crlf = rng.next(2) == 0;
            let sep = if crlf { "\r\n" } else { "\n" };
            let bom = rng.next(3) == 0;
            let text = format!("{}{}{sep}", if bom { "\u{feff}" } else { "" }, lines.join(sep));
            let from = rng.next(n);
            let to = (from + rng.next(3)).min(n - 1);
            let old = lines[from..=to].join(if rng.next(2) == 0 { "\n" } else { "\r\n" });
            let new_lines: Vec<String> = (0..1 + rng.next(3)).map(|k| format!("new {round} {k}")).collect();
            let got = apply_edits(&text, &[edit(&old, &new_lines.join("\n"))], "f").unwrap_or_else(|e| panic!("round {round}: {e}\n{text:?}\n{old:?}"));
            assert_eq!(got.starts_with('\u{feff}'), bom, "round {round}");
            let alone = got.replace("\r\n", "").matches('\n').count();
            let lone_cr = got.replace("\r\n", "").matches('\r').count();
            assert_eq!(lone_cr, 0, "round {round}: {got:?}");
            if crlf {
                assert_eq!(alone, 0, "round {round}: a CRLF file has a bare LF now: {got:?}");
            } else {
                assert_eq!(got.matches('\r').count(), 0, "round {round}: an LF file has a CR now: {got:?}");
            }
            let mut want_lines = lines.clone();
            want_lines.splice(from..=to, new_lines.iter().cloned());
            let want = format!("{}{}{sep}", if bom { "\u{feff}" } else { "" }, want_lines.join(sep));
            assert_eq!(got, want, "round {round}");
        }
    }

    // ---- ED-03: the tolerant ladder ---------------------------------------------------------------------------------------

    fn kind_of(text: &str, old: &str, new: &str) -> Result<(MatchKind, String), String> {
        let mut doc = Doc::parse(text);
        let (o, n) = clean_pair(old, new);
        match locate_unique(doc.text(), &o, &n) {
            Ok(hit) => {
                let kind = hit.kind;
                doc.apply(&hit);
                Ok((kind, doc.render()))
            }
            Err(m) => Err(m.message().to_string()),
        }
    }

    #[test]
    fn the_line_numbers_of_a_copied_read_are_taken_off() {
        let text = "fn a() {\n    1\n}\n\nfn b() {}\n";
        let (kind, got) = kind_of(text, "1\tfn a() {\n2\t    1\n3\t}", "fn a() {\n    2\n}").unwrap();
        assert_eq!(kind, MatchKind::LineNumbers);
        assert_eq!(got, "fn a() {\n    2\n}\n\nfn b() {}\n");
        // A `new` that has them too is stripped; one that has none is left alone.
        let (_, got) = kind_of(text, "5\tfn b() {}", "5\tfn b() { 1 }").unwrap();
        assert_eq!(got, "fn a() {\n    1\n}\n\nfn b() { 1 }\n");
        // Colon and bar forms.
        assert_eq!(kind_of(text, "5: fn b() {}", "fn b() { 2 }").unwrap().0, MatchKind::LineNumbers);
        assert_eq!(kind_of(text, " 5 | fn b() {}", "fn b() { 3 }").unwrap().0, MatchKind::LineNumbers);
        // The same stripped text twice is an error, not a pick.
        let two = "x = 1\ny\nx = 1\n";
        let e = kind_of(two, "1\tx = 1", "x = 2").unwrap_err();
        assert!(e.contains("2 places") && e.contains("line numbers"), "{e}");
        // A line without a number means it is not a copy of a read: nothing is stripped.
        assert!(kind_of(text, "1\tfn a() {\n    1", "z").is_err());
        // A file whose lines really start with a number and a tab is matched exactly, before this rung is tried.
        let tsv = "1\tone\n2\ttwo\n";
        assert_eq!(kind_of(tsv, "2\ttwo", "2\tTWO").unwrap().0, MatchKind::Exact);
    }

    #[test]
    fn curly_quotes_and_long_dashes_are_read_as_the_plain_ones_the_file_has() {
        let text = "let s = \"hello - world\";\nlet t = 'x';\n";
        let (kind, got) = kind_of(text, "let s = \u{201C}hello \u{2013} world\u{201D};", "let s = \"bye\";").unwrap();
        assert_eq!(kind, MatchKind::Unicode);
        assert_eq!(got, "let s = \"bye\";\nlet t = 'x';\n");
        // The other way: the file has the curly ones and the model wrote plain.
        let curly = "say \u{201C}hi there\u{201D} now\nnext\n";
        let (kind, got) = kind_of(curly, "say \"hi there\" now", "say \"yo there\" now").unwrap();
        assert_eq!(kind, MatchKind::Unicode);
        assert_eq!(got, "say \"yo there\" now\nnext\n");
        // A fragment of a line, with the ellipsis character.
        let (_, got) = kind_of("wait\u{2026} then go on\n", "wait... then go", "wait, then go").unwrap();
        assert_eq!(got, "wait, then go on\n");
        // Two places read alike: an error.
        let two = "say \u{201C}hi there\u{201D} now\nsay \u{201C}hi there\u{201D} now\n";
        assert!(kind_of(two, "say \"hi there\" now", "x").unwrap_err().contains("2 places"));
        // A short text is not folded.
        assert!(kind_of("\u{201C}a\u{201D}\n", "\"a\"", "b").is_err());
    }

    #[test]
    fn fold_never_applies_when_exact_exists_elsewhere() {
        let text = "say \"hi there\" now\nsay \u{201C}hi there\u{201D} now\n";
        let (kind, got) = kind_of(text, "say \"hi there\" now", "say \"yo there\" now").unwrap();
        assert_eq!(kind, MatchKind::Exact, "the exact line wins");
        assert_eq!(got, "say \"yo there\" now\nsay \u{201C}hi there\u{201D} now\n", "the curly line is untouched");
    }

    #[test]
    fn indentation_that_differs_is_ignored_and_the_new_text_follows_the_files() {
        let text = "fn f() {\n    if x {\n        y();\n    }\n}\n";
        let (kind, got) = kind_of(text, "if x {\n    y();\n}", "if x {\n    z();\n    w();\n}").unwrap();
        assert_eq!(kind, MatchKind::Indent);
        assert_eq!(got, "fn f() {\n    if x {\n        z();\n        w();\n    }\n}\n");
        // Less indentation in the file than in the text asked for.
        let flat = "if x {\n    y();\n}\n";
        let (kind, got) = kind_of(flat, "        if x {\n            y();\n        }", "        if x {\n            q();\n        }").unwrap();
        assert_eq!(kind, MatchKind::Indent);
        assert_eq!(got, "if x {\n    q();\n}\n");
        // Two places read alike: an error.
        let two = "  a();\n  b();\nx\n    a();\n    b();\n";
        assert!(kind_of(two, "a();\nb();", "c();").unwrap_err().contains("2 places"));
        // Only the ends of the lines differ: the kind says so.
        assert_eq!(kind_of("a(); \nb();\n", "a();\nb();", "c();\nd();").unwrap().0, MatchKind::Trim);
    }

    #[test]
    fn whitespace_that_differs_between_words_is_ignored_when_one_place_matches() {
        let (kind, got) = kind_of("let   a  =  1;\nrest\n", "let a = 1;", "let a = 2;").unwrap();
        assert_eq!(kind, MatchKind::Whitespace);
        assert_eq!(got, "let a = 2;\nrest\n");
        assert!(kind_of("let   a  =  1;\nlet a    = 1;\n", "let a = 1;", "x").unwrap_err().contains("2 places"));
    }

    #[test]
    fn a_line_break_written_out_as_backslash_n_is_read_as_one_when_the_file_has_no_such_text() {
        let text = "alpha\nbeta gamma delta\nomega\n";
        let (kind, got) = kind_of(text, "alpha\\nbeta gamma delta", "ALPHA\\nBETA gamma delta").unwrap();
        assert_eq!(kind, MatchKind::Escape);
        assert_eq!(got, "ALPHA\nBETA gamma delta\nomega\n");
        let (kind, got) = kind_of("let s = \"quoted text here\";\n", "let s = \\\"quoted text here\\\";", "let s = \\\"changed text here\\\";").unwrap();
        assert_eq!(kind, MatchKind::Escape);
        assert_eq!(got, "let s = \"changed text here\";\n");
        // The file has a `\n` of its own (a string in code): the model's text is taken as written, and a miss stays a miss.
        let code = "print(\"a\\nb\")\nbeta gamma delta\n";
        assert!(kind_of(code, "alpha\\nbeta gamma delta", "x").is_err());
        // Two copies: an error.
        let two = "alpha\nbeta gamma delta\nx\nalpha\nbeta gamma delta\n";
        assert!(kind_of(two, "alpha\\nbeta gamma delta", "z").unwrap_err().contains("2 places"));
        // A short text is not unescaped.
        assert!(kind_of("a\nb\n", "a\\nb", "x").is_err());
    }

    #[test]
    fn an_elision_is_refused_with_a_message_that_says_so() {
        for old in ["fn a() {\n    ...\n}", "fn a() {\n    // ...\n}", "fn a() {\n    /* ... */\n}", "fn a() {\n    # ...\n}", "fn a() {\n\u{2026}\n}", "fn a() {\n    // rest of the function\n}", "fn a() {\n    // unchanged\n}"] {
            let e = apply_edits("fn a() {\n    real();\n}\n", &[edit(old, "x")], "f").unwrap_err();
            assert!(e.contains("elision"), "{old:?}: {e}");
        }
        // Text that really has such a line in the file is matched as it is.
        assert_eq!(apply_edits("a\n    ...\nb\n", &[edit("    ...", "    pass")], "f").unwrap(), "a\n    pass\nb\n");
    }

    #[test]
    fn disproportionate_span_is_refused() {
        // The line is 300 spaces and a word: it matches `old` once the ends are trimmed, but replacing it would remove far more than was asked for.
        let text = format!("{}foo()\nnext\n", " ".repeat(300));
        let e = apply_edits(&text, &[edit("foo() ", "bar()")], "f").unwrap_err();
        assert!(e.contains("not found"), "{e}");
        let text = format!("a b{}\nnext\n", " ".repeat(400));
        assert!(apply_edits(&text, &[edit("a b", "c")], "f").is_ok(), "an exact piece of a long line is still an exact match");
    }

    #[test]
    fn a_miss_shows_the_closest_lines_and_the_first_line_that_differs() {
        let text = "fn one() {}\nfn target() {\n    let value = compute(1);\n    value + 1\n}\nfn two() {}\n";
        let e = apply_edits(text, &[edit("fn target() {\n    let value = compute(2);\n    value + 1\n}", "x")], "f").unwrap_err();
        assert!(e.contains("not found") && e.contains("2\tfn target() {") && e.contains("expected: let value = compute(2);") && e.contains("file has: let value = compute(1);"), "{e}");
        assert!(e.chars().count() < 800, "{e}");
        // Nothing alike: the older excerpt.
        let e = apply_edits("fn add() {}\nfn other() {}\n", &[edit("fn add MISSING", "x")], "f").unwrap_err();
        assert!(e.contains("1: fn add()"), "{e}");
    }

    /// 600 edits of text that is drifted the way a small model drifts it (indentation, trailing spaces, line numbers, curly quotes, escapes) in files whose lines repeat: an edit either lands on
    /// the span it was meant for, or is refused. It is never made somewhere else.
    #[test]
    fn a_drifted_old_text_is_applied_to_its_own_span_or_refused_and_never_to_another() {
        let mut rng = Lcg(23);
        let mut applied = 0;
        let mut refused = 0;
        for round in 0..600 {
            let n = 10 + rng.next(40);
            let file: Vec<String> = (0..n)
                .map(|_| match rng.next(5) {
                    0 => format!("    println!(\"value {}\");", rng.next(6)),
                    1 => format!("let v{} = compute({});", rng.next(12), rng.next(4)),
                    2 => format!("        if v{} > {} {{", rng.next(6), rng.next(3)),
                    3 => "}".to_string(),
                    _ => format!("// note {}", rng.next(30)),
                })
                .collect();
            let from = rng.next(n);
            let to = (from + rng.next(3)).min(n - 1);
            let replacement: Vec<String> = (0..1 + rng.next(2)).map(|k| format!("    replaced({round}, {k});")).collect();
            let text = format!("{}\n", file.join("\n"));
            let want_lines = {
                let mut w = file.clone();
                w.splice(from..=to, replacement.iter().cloned());
                format!("{}\n", w.join("\n"))
            };
            let old_lines: Vec<String> = file[from..=to].to_vec();
            // The drift the model adds to the text it copies.
            let drift = rng.next(6);
            let shown: Vec<String> = old_lines
                .iter()
                .enumerate()
                .map(|(k, l)| match drift {
                    0 => l.clone(),
                    1 => format!("{l}   "),
                    2 => l.trim_start().to_string(),
                    3 => format!("{}\t{l}", from + k + 1),
                    4 => l.replace('"', "\u{201C}"),
                    _ => format!("  {l}"),
                })
                .collect();
            let old = shown.join("\n");
            let places = (0..=n - old_lines.len()).filter(|p| (0..old_lines.len()).all(|k| file[p + k].trim() == old_lines[k].trim())).count();
            match apply_edits(&text, &[edit(&old, &replacement.join("\n"))], "f") {
                Ok(got) => {
                    applied += 1;
                    // The replacement may have been re-indented to the file's, so what must hold is that the lines around the span are the file's and the span is where the old text was.
                    let got_lines: Vec<&str> = got.lines().collect();
                    assert_eq!(got_lines.len(), n - old_lines.len() + replacement.len(), "round {round}: the line count changes by exactly this edit\n{text}\n{old:?}\n{got}");
                    assert!(got_lines[..from].iter().zip(&file).all(|(a, b)| a == b), "round {round}: the lines before the span are the file's");
                    assert!(got_lines[from + replacement.len()..].iter().zip(&file[to + 1..]).all(|(a, b)| a == b), "round {round}: the lines after the span are the file's");
                    if drift != 5 && drift != 2 {
                        assert_eq!(got, want_lines, "round {round}");
                    }
                    assert!(got.contains(&format!("replaced({round}, 0);")), "round {round}");
                    assert!(places >= 1, "round {round}");
                }
                Err(_) => refused += 1,
            }
        }
        assert!(applied > 100 && refused > 50, "the generator produces both kinds: {applied} applied, {refused} refused");
    }

    const DIFF: &str = "diff --git a/src/lib.rs b/src/lib.rs\nindex 83db48f..bf2a1c3 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,5 +1,6 @@ fn add\n fn add(a: i32, b: i32) -> i32 {\n-    a - b\n+    a + b\n }\n \n+// the sum\n fn main() {}\n";

    #[test]
    fn a_unified_diff_is_read_into_files_and_hunks() {
        let files = parse_unified(DIFF).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!((files[0].old_path.as_str(), files[0].new_path.as_str(), files[0].target()), ("a/src/lib.rs", "b/src/lib.rs", "b/src/lib.rs"));
        assert_eq!(files[0].hunks[0].old_start, 1);
        assert_eq!(files[0].hunks[0].lines.len(), 7);
        assert_eq!(files[0].hunks[0].lines[1], Line::Remove("    a - b".into()));
        assert!(files[0].warnings.is_empty(), "the counts are right: {:?}", files[0].warnings);
        let two = format!("{DIFF}--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-x\n+y\n");
        assert_eq!(parse_unified(&two).unwrap().len(), 2);
        let created = parse_unified("--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+one\n+two\n").unwrap();
        assert!(created[0].creates_file() && !created[0].deletes_file());
    }

    #[test]
    fn what_is_not_a_diff_is_an_error_and_not_a_guess() {
        for bad in ["", "just some text", "--- a/x\n+++ b/x\n", "@@ -1 +1 @@\n-a\n+b\n", "--- a/x\n+++ b/x\n@@ nonsense @@\n-a\n+b\n"] {
            assert!(parse_unified(bad).is_err(), "{bad:?}");
        }
        let e = parse_unified("--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n a\n?b\n").unwrap_err();
        assert!(e.contains("not part of a hunk"), "{e}");
    }

    #[test]
    fn a_hunk_header_with_wrong_counts_applies_every_line_it_shows() {
        let text = "one\ntwo\nthree\nfour\n";
        // A surplus `+` line: the header says one added line, the body has two.
        let files = parse_unified("--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-one\n+ONE\n+ONE AND A HALF\n").unwrap();
        assert_eq!(apply_hunks(text, &files[0].hunks, "f").unwrap(), "ONE\nONE AND A HALF\ntwo\nthree\nfour\n");
        assert_eq!(files[0].warnings.len(), 1);
        assert!(files[0].warnings[0].contains("hunk 1") && files[0].warnings[0].contains("header says -1,1 +1,1") && files[0].warnings[0].contains("body has -1,1 +1,2") && files[0].warnings[0].contains("applied as written"), "{:?}", files[0].warnings);
        // A surplus `-` line.
        let files = parse_unified("--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-one\n-two\n+TWO\n").unwrap();
        assert_eq!(apply_hunks(text, &files[0].hunks, "f").unwrap(), "TWO\nthree\nfour\n");
        // Counts that are too large: the hunk ends at the next header, not when the numbers run out.
        let files = parse_unified("--- a/f\n+++ b/f\n@@ -1,9 +1,9 @@\n one\n-two\n+TWO\n three\n").unwrap();
        assert_eq!(apply_hunks(text, &files[0].hunks, "f").unwrap(), "one\nTWO\nthree\nfour\n");
        assert_eq!(files[0].warnings.len(), 1, "{:?}", files[0].warnings);
        // Counts that are too small, with a second hunk after it.
        let files = parse_unified("--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n one\n-two\n+TWO\n three\n@@ -4,1 +4,1 @@\n-four\n+FOUR\n").unwrap();
        assert_eq!(files[0].hunks.len(), 2);
        assert_eq!(apply_hunks(text, &files[0].hunks, "f").unwrap(), "one\nTWO\nthree\nFOUR\n");
        // A hunk that is right says nothing.
        let files = parse_unified("--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n").unwrap();
        assert!(files[0].warnings.is_empty());
    }

    #[test]
    fn blank_lines_in_a_hunk_are_context_and_words_after_it_are_not() {
        let text = "a\n\nb\nc\n";
        // The blank context line has lost its space; the header counts it.
        let files = parse_unified("--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n\n-b\n+B\n").unwrap();
        assert_eq!(files[0].hunks[0].lines.len(), 4);
        assert_eq!(apply_hunks(text, &files[0].hunks, "f").unwrap(), "a\n\nB\nc\n");
        // Blank lines and an explanation after the last numbered line end the hunk.
        let files = parse_unified("--- a/f\n+++ b/f\n@@ -3,1 +3,1 @@\n-b\n+B\n\n\nThis changes b to B.\n").unwrap();
        assert_eq!(files[0].hunks[0].lines, vec![Line::Remove("b".into()), Line::Add("B".into())]);
        assert!(files[0].warnings.is_empty());
        // A removed line that begins with two dashes (a SQL comment) is a removal, not a header.
        let files = parse_unified("--- a/q.sql\n+++ b/q.sql\n@@ -1,2 +1,2 @@\n--- keep this\n-select 1;\n+select 2;\n").unwrap();
        assert_eq!(files[0].hunks[0].lines[0], Line::Remove("-- keep this".into()));
    }

    #[test]
    fn a_hunk_is_found_by_its_context_even_when_its_line_numbers_are_wrong() {
        let original = "fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n\nfn main() {}\n";
        let want = "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n// the sum\nfn main() {}\n";
        let files = parse_unified(DIFF).unwrap();
        assert_eq!(apply_hunks(original, &files[0].hunks, "src/lib.rs").unwrap(), want);
        // The same text pushed down by ten lines the diff does not know about: found by its context.
        let shifted = format!("{}{original}", "// header\n".repeat(10));
        assert_eq!(apply_hunks(&shifted, &files[0].hunks, "src/lib.rs").unwrap(), format!("{}{want}", "// header\n".repeat(10)));
    }

    #[test]
    fn a_hunk_that_matches_in_two_places_far_from_its_line_is_refused() {
        let mut lines: Vec<String> = (0..60).map(|k| format!("filler {k}")).collect();
        lines[10] = "    return Ok(())".into();
        lines[40] = "    return Ok(())".into();
        let text = format!("{}\n", lines.join("\n"));
        // The hunk names line 25, and its context is at lines 11 and 41: neither is near.
        let files = parse_unified("--- a/f\n+++ b/f\n@@ -25,1 +25,1 @@\n-    return Ok(())\n+    return Err(())\n").unwrap();
        let e = apply_hunks(&text, &files[0].hunks, "f").unwrap_err();
        assert!(e.contains("hunk 1") && e.contains("[11, 41]") && e.contains("line 25") && e.contains("add context lines"), "{e}");
        // Naming a line close to one of them is enough to choose it.
        let files = parse_unified("--- a/f\n+++ b/f\n@@ -42,1 +42,1 @@\n-    return Ok(())\n+    return Err(())\n").unwrap();
        let got = apply_hunks(&text, &files[0].hunks, "f").unwrap();
        assert_eq!(got.lines().nth(40), Some("    return Err(())"));
        assert_eq!(got.lines().nth(10), Some("    return Ok(())"));
        // Only one place: it is taken wherever the line number points.
        let files = parse_unified("--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-filler 33\n+changed\n").unwrap();
        assert!(apply_hunks(&text, &files[0].hunks, "f").unwrap().contains("changed"));
    }

    #[test]
    fn a_diff_applies_to_a_file_with_a_byte_order_mark_and_keeps_it() {
        let files = parse_unified("--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n first\n-second\n+SECOND\n").unwrap();
        assert_eq!(apply_hunks("\u{feff}first\r\nsecond\r\n", &files[0].hunks, "f").unwrap(), "\u{feff}first\r\nSECOND\r\n");
        let files = parse_unified("--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-first\n+FIRST\n").unwrap();
        assert_eq!(apply_hunks("\u{feff}first\nrest\n", &files[0].hunks, "f").unwrap(), "\u{feff}FIRST\nrest\n");
    }

    #[test]
    fn a_hunk_whose_context_is_not_there_is_an_error_naming_the_hunk() {
        let files = parse_unified(DIFF).unwrap();
        let e = apply_hunks("something else entirely\n", &files[0].hunks, "src/lib.rs").unwrap_err();
        assert!(e.contains("src/lib.rs, hunk 1") && e.contains("not in the file") && e.contains("fn add"), "{e}");
        // One that matches only after trailing whitespace is ignored still applies (a model drops it).
        let spaced = "fn add(a: i32, b: i32) -> i32 {  \n    a - b \n}\n\nfn main() {}\n";
        assert!(apply_hunks(spaced, &files[0].hunks, "f").unwrap().contains("a + b"));
    }

    #[test]
    fn crlf_files_stay_crlf_and_a_file_without_a_final_newline_stays_that_way() {
        let files = parse_unified("--- a/x\n+++ b/x\n@@ -1,2 +1,3 @@\n one\n-two\n+TWO\n+two and a half\n").unwrap();
        assert_eq!(apply_hunks("one\r\ntwo\r\n", &files[0].hunks, "x").unwrap(), "one\r\nTWO\r\ntwo and a half\r\n");
        assert_eq!(apply_hunks("one\ntwo", &files[0].hunks, "x").unwrap(), "one\nTWO\ntwo and a half");
    }

    #[test]
    fn several_hunks_in_one_file_keep_their_places_as_earlier_ones_grow_or_shrink_it() {
        let diff = "--- a/f\n+++ b/f\n@@ -1,3 +1,4 @@\n a\n+a2\n b\n c\n@@ -8,3 +9,2 @@\n h\n-i\n j\n";
        let text = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n";
        let files = parse_unified(diff).unwrap();
        assert_eq!(apply_hunks(text, &files[0].hunks, "f").unwrap(), "a\na2\nb\nc\nd\ne\nf\ng\nh\nj\nk\n");
    }

    #[test]
    fn a_hunk_of_only_additions_goes_after_the_line_it_names() {
        let files = parse_unified("--- a/f\n+++ b/f\n@@ -2,0 +3,2 @@\n+new1\n+new2\n").unwrap();
        assert_eq!(apply_hunks("a\nb\nc\n", &files[0].hunks, "f").unwrap(), "a\nb\nnew1\nnew2\nc\n");
        // Past the end it appends instead of failing.
        let files = parse_unified("--- a/f\n+++ b/f\n@@ -99,0 +100,1 @@\n+tail\n").unwrap();
        assert_eq!(apply_hunks("a\n", &files[0].hunks, "f").unwrap(), "a\ntail\n");
    }

    /// Writes the diff that turns `old` into `new` where only the lines `[from, to)` of `old` change, with `ctx` lines of context, and
    /// states the start `skew` lines away from the truth.
    fn make_diff(old: &[String], from: usize, to: usize, replacement: &[String], ctx: usize, skew: isize) -> String {
        let start = from.saturating_sub(ctx);
        let end = (to + ctx).min(old.len());
        let mut body = String::new();
        for l in &old[start..from] {
            body.push_str(&format!(" {l}\n"));
        }
        for l in &old[from..to] {
            body.push_str(&format!("-{l}\n"));
        }
        for l in replacement {
            body.push_str(&format!("+{l}\n"));
        }
        for l in &old[to..end] {
            body.push_str(&format!(" {l}\n"));
        }
        let old_len = end - start;
        let new_len = old_len - (to - from) + replacement.len();
        let declared = (start as isize + 1 + skew).max(1);
        format!("--- a/f\n+++ b/f\n@@ -{declared},{old_len} +{declared},{new_len} @@\n{body}")
    }

    #[test]
    fn random_hunks_apply_exactly_and_a_hunk_that_does_not_match_changes_nothing() {
        let mut rng = Lcg(7);
        let mut applied = 0;
        for round in 0..600 {
            // Lines drawn from a few words, so the same line (and the same context) occurs again and again.
            let n = 8 + rng.next(60);
            let old: Vec<String> = (0..n).map(|_| format!("line {}", rng.next(6))).collect();
            let from = rng.next(n);
            let to = (from + rng.next(4)).min(n);
            let replacement: Vec<String> = (0..rng.next(4)).map(|k| format!("new {round} {k}")).collect();
            let ctx = 1 + rng.next(3);
            let skew = rng.next(7) as isize - 3;
            let text = format!("{}\n", old.join("\n"));
            let diff = make_diff(&old, from, to, &replacement, ctx, skew);
            let files = parse_unified(&diff).unwrap_or_else(|e| panic!("round {round}: {e}\n{diff}"));
            let mut expected: Vec<String> = old.clone();
            expected.splice(from..to, replacement.iter().cloned());
            let expected = format!("{}\n", expected.join("\n"));
            // How often the lines the hunk expects occur in the file: with one place there is one answer.
            let (cut_from, cut_to) = (from.saturating_sub(ctx), (to + ctx).min(n));
            let pattern: Vec<&String> = old[cut_from..from].iter().chain(old[from..to].iter()).chain(old[to..cut_to].iter()).collect();
            let places = (0..=n - pattern.len()).filter(|p| pattern.iter().enumerate().all(|(k, l)| &old[p + k] == *l)).count();
            let got = apply_hunks(&text, &files[0].hunks, "f").unwrap_or_else(|e| panic!("round {round}: a hunk cut from the file itself must apply: {e}\n{diff}"));
            if places == 1 {
                assert_eq!(got, expected, "round {round}\n{diff}");
            } else {
                // The same lines occur in several places: the hunk goes to the one nearest where it says, which may not be where it was cut.
                // What must hold is that the file changes by exactly this hunk's change, and nowhere else.
                let line_delta = replacement.len() as isize - (to - from) as isize;
                assert_eq!(got.matches('\n').count() as isize, text.matches('\n').count() as isize + line_delta, "round {round}: the line count changes by exactly the hunk's change");
                assert!(replacement.iter().all(|r| got.contains(r.as_str())), "round {round}: the new lines are there");
            }
            applied += 1;
            // The same hunk against text that does not contain its context is refused.
            assert!(apply_hunks("nothing like it here\nnor here\n", &files[0].hunks, "f").is_err(), "round {round}");
        }
        assert_eq!(applied, 600);
    }

    #[test]
    fn the_lines_a_hunk_keeps_are_the_files_own_and_a_hunk_may_forgive_spaces_when_it_still_finds_one_place() {
        // Trailing spaces of the file's context lines stay (the diff did not mean to change them).
        let files = parse_unified("--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n line1\n-line2\n+changed\n line3\n").unwrap();
        assert_eq!(apply_hunks("line1  \nline2\nline3   \n", &files[0].hunks, "f").unwrap(), "line1  \nchanged\nline3   \n");
        // Indentation the hunk does not have: found when there is one place, and the added line follows the file's indentation.
        let files = parse_unified("--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-a();\n+c();\n").unwrap();
        assert_eq!(apply_hunks("    a();\n    b();\n", &files[0].hunks, "f").unwrap(), "    c();\n    b();\n");
        let e = apply_hunks("    a();\n    b();\n  a();\n", &files[0].hunks, "f").unwrap_err();
        assert!(e.contains("2 places") && e.contains("ignoring indentation") && e.contains("add context lines"), "{e}");
        // Curly quotes and long dashes in the file where the diff has plain ones.
        let files = parse_unified("--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-He said \"hello\"\n+He said \"hi\"\n").unwrap();
        assert_eq!(apply_hunks("He said \u{201C}hello\u{201D}\nsome\u{2014}dash\nend\n", &files[0].hunks, "f").unwrap(), "He said \"hi\"\nsome\u{2014}dash\nend\n");
        // An exact place is preferred over a forgiving one elsewhere.
        let files = parse_unified("--- a/f\n+++ b/f\n@@ -3,1 +3,1 @@\n-x = 1\n+x = 2\n").unwrap();
        assert_eq!(apply_hunks("  x = 1\ny\nx = 1\n", &files[0].hunks, "f").unwrap(), "  x = 1\ny\nx = 2\n");
    }
}

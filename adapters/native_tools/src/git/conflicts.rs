//! Merge conflicts: found, described and resolved by an edit the person approves (card OA7-05, TM-18).
//!
//! A merge, a stash that is applied, a cherry-pick or a rebase that someone started leaves files with conflict markers in them. `git.conflicts` reads the unmerged files and, for each one that
//! is text, the hunks between the markers, and says them as text from outside (the host fences every git result). `git.resolve_conflict` replaces the hunks of ONE file by one side, both
//! sides or text the model wrote, as an edit: the person sees the change as a diff before they say yes (`approval_preview`), the file is kept in the undo store first, nothing is staged and
//! nothing is committed. The commit gate (`gate.rs`) refuses what still holds markers.
//!
//! This file is the part that reads and rewrites text; it opens no file and starts no program.

/// The marker lines git writes: seven characters, then a space and a label (or the end of the line).
const OURS: &str = "<<<<<<<";
const BASE: &str = "|||||||";
const MIDDLE: &str = "=======";
const THEIRS: &str = ">>>>>>>";

/// The most a file may be to be read for hunks.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// What one conflicted region is: where it stands, how each side calls itself, and the lines of each side as they are in the file (with their line ends).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// 1 for the first hunk of the file.
    pub index: usize,
    /// The line numbers (1-based) of the opening and the closing marker.
    pub start: usize,
    pub end: usize,
    pub ours_label: String,
    pub theirs_label: String,
    /// The label of the common ancestor, when the repository writes it (`merge.conflictStyle=diff3`).
    pub base_label: Option<String>,
    pub ours: Vec<String>,
    pub base: Option<Vec<String>>,
    pub theirs: Vec<String>,
    /// The marker lines as they are in the file, so that a hunk that is not resolved is written back exactly as it was.
    open: String,
    base_mark: Option<String>,
    middle: String,
    close: String,
}

impl Hunk {
    /// The line end of this hunk: the one its own lines use (ours, then theirs), else the one of its markers. (A file of Windows line ends may have markers git wrote with `\n`.)
    fn eol(&self) -> &'static str {
        let crlf = |l: &String| l.ends_with("\r\n");
        let lf = |l: &String| l.ends_with('\n') && !l.ends_with("\r\n");
        let own = self.ours.iter().chain(self.theirs.iter());
        if own.clone().any(crlf) {
            "\r\n"
        } else if own.clone().any(lf) {
            "\n"
        } else if self.open.ends_with("\r\n") {
            "\r\n"
        } else {
            "\n"
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Lines outside every hunk.
    Text(Vec<String>),
    Hunk(Box<Hunk>),
}

/// A file read for conflict markers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Parsed {
    pub segments: Vec<Segment>,
    /// The line where the markers stop making sense (a second opening marker, a closing one with no opening one, the end of the file inside a hunk), when they do.
    pub unbalanced: Option<usize>,
}

impl Parsed {
    pub fn hunks(&self) -> Vec<&Hunk> {
        self.segments.iter().filter_map(|s| if let Segment::Hunk(h) = s { Some(&**h) } else { None }).collect()
    }
}

/// The text of a marker line without its line end.
fn bare(line: &str) -> &str {
    line.trim_end_matches(['\n', '\r'])
}

/// Is `line` the marker `m` (seven characters, and then the end of the line or a space)? Returns what follows the marker (the label).
fn marker<'a>(line: &'a str, m: &str) -> Option<&'a str> {
    let b = bare(line);
    let rest = b.strip_prefix(m)?;
    if rest.is_empty() {
        Some("")
    } else {
        rest.strip_prefix(' ').map(str::trim)
    }
}

/// Reads the hunks of a file. A file with no markers is one text segment. A line that is a marker of the wrong kind for where the reading is (an opening marker inside a hunk, a closing one
/// outside) stops the reading there: `unbalanced` says the line, the lines from the start of that hunk on are kept as text, and the file is not one `resolve` will change.
pub fn parse(text: &str) -> Parsed {
    /// The part of an open hunk the reading is in.
    #[derive(PartialEq)]
    enum Side {
        Ours,
        Base,
        Theirs,
    }
    let mut segments: Vec<Segment> = Vec::new();
    let mut plain: Vec<String> = Vec::new();
    // The hunk being read, with the part of it the reading is in; nothing open is the text between hunks. The hunk travels with its part, so there is no part without a hunk.
    let mut open: Option<(Hunk, Side)> = None;
    // The lines of the hunk being read, in case it turns out not to end: they go back to the text.
    let mut raw: Vec<String> = Vec::new();
    let mut unbalanced = None;
    let mut count = 0usize;
    for (i, line) in text.split_inclusive('\n').enumerate() {
        let no = i + 1;
        if unbalanced.is_some() {
            plain.push(line.to_string());
            continue;
        }
        // Between hunks: the line is text, or it opens a hunk, or it is a closing marker with nothing open.
        let Some((mut h, mut side)) = open.take() else {
            if let Some(label) = marker(line, OURS) {
                if !plain.is_empty() {
                    segments.push(Segment::Text(std::mem::take(&mut plain)));
                }
                count += 1;
                raw = vec![line.to_string()];
                open = Some((Hunk { index: count, start: no, end: no, ours_label: label.to_string(), theirs_label: String::new(), base_label: None, ours: Vec::new(), base: None, theirs: Vec::new(), open: line.to_string(), base_mark: None, middle: String::new(), close: String::new() }, Side::Ours));
            } else if marker(line, THEIRS).is_some() {
                // A closing marker with nothing open: the markers do not balance. (A line of equals signs alone is text: a heading's underline.)
                unbalanced = Some(no);
                plain.push(line.to_string());
            } else {
                plain.push(line.to_string());
            }
            continue;
        };
        // Inside a hunk.
        raw.push(line.to_string());
        if marker(line, OURS).is_some() {
            unbalanced = Some(h.start);
        } else if side != Side::Theirs && marker(line, BASE).is_some() {
            if side == Side::Base {
                unbalanced = Some(h.start);
            } else {
                h.base_label = marker(line, BASE).map(str::to_string);
                h.base_mark = Some(line.to_string());
                h.base = Some(Vec::new());
                side = Side::Base;
            }
        } else if side != Side::Theirs && bare(line) == MIDDLE {
            h.middle = line.to_string();
            side = Side::Theirs;
        } else if side == Side::Theirs && marker(line, THEIRS).is_some() {
            h.theirs_label = marker(line, THEIRS).unwrap_or("").to_string();
            h.close = line.to_string();
            h.end = no;
            segments.push(Segment::Hunk(Box::new(h)));
            raw.clear();
            continue;
        } else if side == Side::Theirs && (bare(line) == MIDDLE || marker(line, BASE).is_some()) {
            unbalanced = Some(h.start);
        } else {
            match side {
                Side::Ours => h.ours.push(line.to_string()),
                Side::Base => h.base.get_or_insert_with(Vec::new).push(line.to_string()),
                Side::Theirs => h.theirs.push(line.to_string()),
            }
        }
        if unbalanced.is_some() {
            // The hunk is not one: its lines are text again, and so is everything after it.
            plain.append(&mut raw);
        } else {
            open = Some((h, side));
        }
    }
    if let Some((h, _)) = open.take() {
        // The end of the file inside a hunk.
        unbalanced = Some(h.start);
        plain.append(&mut raw);
    }
    if !plain.is_empty() {
        segments.push(Segment::Text(plain));
    }
    Parsed { segments, unbalanced }
}

/// What to put where a hunk was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Take {
    Ours,
    Theirs,
    /// Ours, then theirs.
    Both,
    /// This text instead (an empty text takes the hunk out).
    Text(String),
}

impl Take {
    pub fn word(&self) -> &'static str {
        match self {
            Take::Ours => "ours",
            Take::Theirs => "theirs",
            Take::Both => "both",
            Take::Text(_) => "text",
        }
    }
}

/// The text a file becomes when the hunks named in `choices` (hunk number, what to take) are replaced, and how many hunks it still holds. The other hunks are written as they were.
pub fn resolve(parsed: &Parsed, choices: &[(usize, Take)]) -> Result<(String, usize), String> {
    if let Some(line) = parsed.unbalanced {
        return Err(format!("the markers of this file do not balance (see line {line}): it has to be fixed by hand"));
    }
    let hunks = parsed.hunks();
    for (n, _) in choices {
        if *n == 0 || *n > hunks.len() {
            return Err(format!("this file has {} conflict hunk{}; there is no hunk {n}", hunks.len(), if hunks.len() == 1 { "" } else { "s" }));
        }
    }
    for (i, (n, _)) in choices.iter().enumerate() {
        if choices[..i].iter().any(|(m, _)| m == n) {
            return Err(format!("hunk {n} is named twice"));
        }
    }
    let mut out = String::new();
    let mut left = 0usize;
    for seg in &parsed.segments {
        match seg {
            Segment::Text(lines) => lines.iter().for_each(|l| out.push_str(l)),
            Segment::Hunk(h) => match choices.iter().find(|(n, _)| *n == h.index).map(|(_, t)| t) {
                None => {
                    left += 1;
                    out.push_str(&h.open);
                    h.ours.iter().for_each(|l| out.push_str(l));
                    if let (Some(mark), Some(base)) = (&h.base_mark, &h.base) {
                        out.push_str(mark);
                        base.iter().for_each(|l| out.push_str(l));
                    }
                    out.push_str(&h.middle);
                    h.theirs.iter().for_each(|l| out.push_str(l));
                    out.push_str(&h.close);
                }
                Some(Take::Ours) => h.ours.iter().for_each(|l| out.push_str(l)),
                Some(Take::Theirs) => h.theirs.iter().for_each(|l| out.push_str(l)),
                Some(Take::Both) => {
                    h.ours.iter().for_each(|l| out.push_str(l));
                    h.theirs.iter().for_each(|l| out.push_str(l));
                }
                Some(Take::Text(text)) => {
                    let eol = h.eol();
                    let body = text.replace("\r\n", "\n");
                    if !body.is_empty() {
                        out.push_str(&body.replace('\n', eol));
                        if !body.ends_with('\n') {
                            out.push_str(eol);
                        }
                    }
                }
            },
        }
    }
    Ok((out, left))
}

/// Does `text` hold a whole conflict (an opening marker, a middle one and a closing one, in that order)? What the commit gate asks of the lines a commit adds.
pub fn holds_markers(text: &str) -> Option<usize> {
    let mut open: Option<usize> = None;
    let mut middle = false;
    for (i, line) in text.lines().enumerate() {
        if marker(line, OURS).is_some() {
            open = Some(i + 1);
            middle = false;
        } else if open.is_some() && bare(line) == MIDDLE {
            middle = true;
        } else if open.is_some() && middle && marker(line, THEIRS).is_some() {
            return open;
        }
    }
    None
}

/// How a path is conflicted, from the stages the index holds for it: 1 the common ancestor, 2 ours, 3 theirs (what `git ls-files -u` lists).
pub fn kind_of(stages: u8) -> &'static str {
    match stages {
        0b111 => "both modified",
        0b110 => "both added",
        0b011 => "deleted by them, modified by us",
        0b101 => "deleted by us, modified by them",
        0b010 => "added by us",
        0b100 => "added by them",
        0b001 => "deleted by both",
        _ => "unmerged",
    }
}

/// The lines of a side for a person to read: at most `lines` of them and `chars` characters of each, with what was left out said.
pub fn excerpt(side: &[String], lines: usize, chars: usize) -> String {
    let mut out: Vec<String> = side.iter().take(lines).map(|l| {
        let b = bare(l);
        if b.chars().count() > chars {
            format!("{}...", b.chars().take(chars).collect::<String>())
        } else {
            b.to_string()
        }
    }).collect();
    if side.len() > lines {
        out.push(format!("... {} more line{}", side.len() - lines, if side.len() - lines == 1 { "" } else { "s" }));
    }
    if out.is_empty() {
        out.push("(nothing)".to_string());
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MERGE: &str = "top\n<<<<<<< HEAD\nours 1\nours 2\n=======\ntheirs 1\n>>>>>>> feature\nmiddle\n<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> feature\nend\n";

    #[test]
    fn the_hunks_of_a_merge_are_found_with_their_lines_and_labels() {
        let p = parse(MERGE);
        assert_eq!(p.unbalanced, None);
        let h = p.hunks();
        assert_eq!(h.len(), 2);
        assert_eq!((h[0].index, h[0].start, h[0].end), (1, 2, 7));
        assert_eq!((h[0].ours_label.as_str(), h[0].theirs_label.as_str()), ("HEAD", "feature"));
        assert_eq!(h[0].ours, vec!["ours 1\n", "ours 2\n"]);
        assert_eq!(h[0].theirs, vec!["theirs 1\n"]);
        assert_eq!((h[1].start, h[1].end), (9, 13));
        assert_eq!(h[0].base, None);
    }

    #[test]
    fn a_diff3_hunk_has_a_base_and_a_file_without_markers_has_none() {
        let p = parse("a\n<<<<<<< ours\nO\n||||||| base\nB\n=======\nT\n>>>>>>> theirs\nz\n");
        let h = p.hunks();
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].base_label.as_deref(), h[0].base.clone()), (Some("base"), Some(vec!["B\n".to_string()])));
        assert!(parse("just\ntext\n").hunks().is_empty());
        assert_eq!(parse("").segments.len(), 0);
    }

    #[test]
    fn markers_that_do_not_balance_are_said_and_nothing_is_taken_for_a_hunk() {
        // An opening marker inside a hunk, a closing marker with none open, the end of the file in a hunk.
        for (text, line) in [("<<<<<<< a\nx\n<<<<<<< b\n", 1usize), ("x\n>>>>>>> b\n", 2), ("a\n<<<<<<< a\nx\n=======\ny\n", 2), ("<<<<<<< a\n=======\n=======\n>>>>>>> b\n", 1)] {
            let p = parse(text);
            assert_eq!(p.unbalanced, Some(line), "{text:?}");
            assert!(p.hunks().is_empty() || text.starts_with("a\n"), "{text:?}");
            assert!(resolve(&p, &[(1, Take::Ours)]).is_err());
            // The text is all still there.
            let all: String = p.segments.iter().map(|s| match s { Segment::Text(l) => l.concat(), Segment::Hunk(_) => String::new() }).collect();
            assert_eq!(all, text, "{text:?}");
        }
        // A line of equals signs and a longer run of arrows are text.
        assert!(parse("Title\n=======\nbody\n<<<<<<<< not a marker\n").hunks().is_empty());
        assert_eq!(parse("Title\n=======\nbody\n").unbalanced, None);
    }

    #[test]
    fn a_hunk_is_replaced_by_a_side_both_or_text_and_the_others_are_written_back_as_they_were() {
        let p = parse(MERGE);
        let (ours, left) = resolve(&p, &[(1, Take::Ours), (2, Take::Ours)]).unwrap();
        assert_eq!((ours.as_str(), left), ("top\nours 1\nours 2\nmiddle\nx\nend\n", 0));
        let (theirs, _) = resolve(&p, &[(1, Take::Theirs), (2, Take::Theirs)]).unwrap();
        assert_eq!(theirs, "top\ntheirs 1\nmiddle\ny\nend\n");
        let (both, _) = resolve(&p, &[(1, Take::Both), (2, Take::Text("z".into()))]).unwrap();
        assert_eq!(both, "top\nours 1\nours 2\ntheirs 1\nmiddle\nz\nend\n");
        // One hunk decided, one left: the second keeps its markers byte for byte.
        let (partial, left) = resolve(&p, &[(1, Take::Theirs)]).unwrap();
        assert_eq!(left, 1);
        assert!(partial.starts_with("top\ntheirs 1\nmiddle\n<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> feature\nend\n"), "{partial}");
        // No decision at all changes nothing.
        assert_eq!(resolve(&p, &[]).unwrap(), (MERGE.to_string(), 2));
        // An empty text takes the hunk out.
        assert_eq!(resolve(&p, &[(1, Take::Text(String::new())), (2, Take::Text(String::new()))]).unwrap().0, "top\nmiddle\nend\n");
    }

    #[test]
    fn a_hunk_that_is_not_there_or_is_named_twice_is_refused() {
        let p = parse(MERGE);
        assert!(resolve(&p, &[(3, Take::Ours)]).unwrap_err().contains("no hunk 3"));
        assert!(resolve(&p, &[(0, Take::Ours)]).is_err());
        assert!(resolve(&p, &[(1, Take::Ours), (1, Take::Theirs)]).unwrap_err().contains("twice"));
    }

    #[test]
    fn line_ends_are_kept_and_text_is_written_with_the_ends_of_the_file() {
        let crlf = "a\r\n<<<<<<< HEAD\r\nO\r\n=======\r\nT\r\n>>>>>>> x\r\nz\r\n";
        let p = parse(crlf);
        assert_eq!(resolve(&p, &[(1, Take::Ours)]).unwrap().0, "a\r\nO\r\nz\r\n");
        assert_eq!(resolve(&p, &[(1, Take::Text("one\ntwo".into()))]).unwrap().0, "a\r\none\r\ntwo\r\nz\r\n", "the text takes the ends of the file's markers");
        assert_eq!(resolve(&p, &[(1, Take::Text("one\r\ntwo\r\n".into()))]).unwrap().0, "a\r\none\r\ntwo\r\nz\r\n");
        // A side that is the last line of a file with no final line end.
        let p = parse("<<<<<<< a\nO\n=======\nT\n>>>>>>> b");
        assert_eq!(resolve(&p, &[(1, Take::Theirs)]).unwrap().0, "T\n");
    }

    #[test]
    fn a_whole_conflict_is_what_the_commit_gate_looks_for_and_one_marker_alone_is_not() {
        assert_eq!(holds_markers("x\n<<<<<<< HEAD\na\n=======\nb\n>>>>>>> f\n"), Some(2));
        assert_eq!(holds_markers("<<<<<<< a\nx\n||||||| b\ny\n=======\nz\n>>>>>>> c\n"), Some(1));
        // A heading underline, or arrows in a document, are not a conflict.
        assert_eq!(holds_markers("Title\n=======\nbody\n"), None);
        assert_eq!(holds_markers("<<<<<<< HEAD\nonly the start\n"), None);
        assert_eq!(holds_markers("=======\n>>>>>>> x\n"), None);
        assert_eq!(holds_markers(""), None);
    }

    #[test]
    fn the_kind_of_a_conflict_is_said_in_words_and_a_side_is_cut_for_a_person_to_read() {
        assert_eq!(kind_of(0b111), "both modified");
        assert_eq!(kind_of(0b110), "both added");
        assert_eq!(kind_of(0b011), "deleted by them, modified by us");
        assert_eq!(kind_of(0b101), "deleted by us, modified by them");
        let side: Vec<String> = (1..=5).map(|i| format!("line {i}\r\n")).collect();
        assert_eq!(excerpt(&side, 3, 80), "line 1\nline 2\nline 3\n... 2 more lines");
        assert_eq!(excerpt(&[], 3, 80), "(nothing)");
        assert_eq!(excerpt(&["x".repeat(10) + "\n"], 3, 4), "xxxx...");
    }

    /// What `parse` makes of a file, as a short list: `T3` is a text segment of three lines, `H2-6` a hunk from line 2 to line 6.
    fn shape(p: &Parsed) -> Vec<String> {
        p.segments.iter().map(|s| match s {
            Segment::Text(lines) => format!("T{}", lines.len()),
            Segment::Hunk(h) => format!("H{}-{}", h.start, h.end),
        }).collect()
    }

    #[test]
    fn markers_in_an_odd_order_never_panic_and_are_read_the_way_they_always_were() {
        // (the file, the shape `parse` gives it, the line where the markers stop making sense). The shapes were taken from the parser as it was before the open hunk was carried in its state.
        let cases: [(&str, &[&str], Option<usize>); 14] = [
            // A closing marker with no opening one, alone and after other lines; a middle marker alone is a heading's underline.
            (">>>>>>> x\n", &["T1"], Some(1)),
            ("=======\n>>>>>>> x\n", &["T2"], Some(2)),
            ("a\n=======\n>>>>>>> x\nb\n", &["T4"], Some(3)),
            ("=======\nbody\n", &["T2"], None),
            // A closing marker before the middle one is a line of the hunk, and the file ends inside it.
            ("<<<<<<< a\n>>>>>>> b\n=======\n", &["T3"], Some(1)),
            ("<<<<<<< a\no\n>>>>>>> b\n", &["T3"], Some(1)),
            // The end of the file inside a hunk: its lines go back to the text, after the lines that came before it.
            ("x\n<<<<<<< a\no\n=======\nt\n", &["T1", "T4"], Some(2)),
            ("<<<<<<< a", &["T1"], Some(1)),
            // A middle or a base marker where it does not belong, and an opening marker inside a hunk.
            ("<<<<<<< a\no\n=======\nt\n||||||| b\n>>>>>>> c\n", &["T6"], Some(1)),
            ("<<<<<<< a\n||||||| b\nB\n||||||| c\nC\n=======\nt\n>>>>>>> d\n", &["T8"], Some(1)),
            ("<<<<<<< a\no\n=======\nt\n<<<<<<< b\n", &["T5"], Some(1)),
            // A hunk that is whole, then a closing marker with none open: the hunk stays a hunk.
            ("<<<<<<< a\no\n=======\nt\n>>>>>>> b\n>>>>>>> c\n", &["H1-5", "T1"], Some(6)),
            // Lines outside every hunk that look like markers of the middle of one are text.
            ("=======\n<<<<<<< a\no\n=======\nt\n>>>>>>> b\n", &["T1", "H2-6"], None),
            ("<<<<<<< a\n=======\n>>>>>>> b\n||||||| c\n", &["H1-3", "T1"], None),
        ];
        for (text, want, unbalanced) in cases {
            let p = parse(text);
            assert_eq!((shape(&p), p.unbalanced), (want.iter().map(|s| s.to_string()).collect::<Vec<_>>(), unbalanced), "{text:?}");
            // Whatever the markers are, every line of the file is still there, in its order.
            let mut all = String::new();
            for s in &p.segments {
                match s {
                    Segment::Text(l) => all.push_str(&l.concat()),
                    Segment::Hunk(h) => {
                        all.push_str(&h.open);
                        h.ours.iter().for_each(|l| all.push_str(l));
                        if let (Some(mark), Some(base)) = (&h.base_mark, &h.base) {
                            all.push_str(mark);
                            base.iter().for_each(|l| all.push_str(l));
                        }
                        all.push_str(&h.middle);
                        h.theirs.iter().for_each(|l| all.push_str(l));
                        all.push_str(&h.close);
                    }
                }
            }
            assert_eq!(all, text, "{text:?}");
            // A file whose markers do not balance is one `resolve` refuses; one that balances is written back as it was when nothing is decided.
            match unbalanced {
                Some(_) => assert!(resolve(&p, &[(1, Take::Ours)]).is_err(), "{text:?}"),
                None => assert_eq!(resolve(&p, &[]).unwrap().0, text, "{text:?}"),
            }
        }
    }
}

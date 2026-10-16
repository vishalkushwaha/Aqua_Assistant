//! Saying a change in words (card OA10-06; task UX-16 of the OpenCode audit).
//!
//! A person who codes by voice cannot look at the screen all the time, and a diff read out letter by letter is no use to them. This module turns what a run changed (the patches of
//! `GET /api/v1/runs/{id}/diff`) into short sentences, and says paths, symbols and lines of code the way a person would say them. It reads text and gives text: nothing here reads a file,
//! runs a program or decides what may be shown. The caller gives it patches that were already masked (the diff route masks every patch like a tool result) and it masks what it says once more
//! (`coding_voice::mask_spoken`), because what is spoken may be heard by someone else in the room.
//!
//! * [`summarise`]: "3 files changed, 14 lines added and 6 removed: main.rs, lib.rs and util.rs." For a large change only that, and a hint to ask for one file.
//! * [`describe_file`] and [`describe_hunk`]: "main.rs: replaced the call to parse with parse strict in function load, lines 40 to 52." The place is the function the hunk header names
//!   (git writes it after the second `@@`), the lines are the lines that changed, not the lines around them, and a one-word swap is said as a swap.
//! * [`spell_hunk`]: the line-by-line mode, for when the person asks for it: each changed line with its number, identifiers as the words they are (`parse_strict` is "parse strict",
//!   `parseStrict` too) and punctuation by name ("open paren", "double equals", "arrow").
//! * [`say_path`], [`say_file_name`], [`say_symbol`], [`say_code`]: how a path, a file name, a name in code and a line of code are said. A file is "main dot R S": the text-to-speech engine
//!   reads "main.rs" as nothing a person would write.
//!
//! Limits, so that nothing is read out for minutes: [`MAX_FILES_NAMED`] files by name, [`BIG_DIFF_LINES`] changed lines (or [`BIG_DIFF_FILES`] files) before only the summary is given,
//! [`MAX_SPELLED_LINES`] lines in the line-by-line mode, and [`MAX_SPOKEN_LINE`] characters of one spelled line.

use serde_json::Value;

/// How many files are named in a summary; the rest are counted.
pub const MAX_FILES_NAMED: usize = 6;
/// A change of more changed lines than this, or of more files than [`BIG_DIFF_FILES`], is only summarised: the person asks for the part they want.
pub const BIG_DIFF_LINES: usize = 400;
pub const BIG_DIFF_FILES: usize = 8;
/// How many changed lines the line-by-line mode says.
pub const MAX_SPELLED_LINES: usize = 30;
/// A spelled line is cut at this many characters.
pub const MAX_SPOKEN_LINE: usize = 200;
/// How many changes of one file are described.
const MAX_HUNKS_PER_FILE: usize = 6;
/// The path of a file is said from its last three parts.
const PATH_PARTS: usize = 3;

/// What a run did to one file, as the diff route gives it: the patch is the text of `git diff` for the file from its first `---` line (masked), empty for a file that is only named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    /// `A` added, `D` deleted, `M` changed (as git writes them; anything else is read as changed).
    pub status: char,
    pub patch: String,
    /// The patch was cut at the limit of the diff route.
    pub truncated: bool,
    pub binary: bool,
}

/// One change of a file: the hunk of a patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    /// What git writes after the second `@@`: the line above the hunk that starts a function or a class, when it found one.
    pub context: String,
    /// The lines, each with its mark: `+`, `-` or a space.
    pub lines: Vec<(char, String)>,
}

impl Hunk {
    pub fn added(&self) -> usize {
        self.lines.iter().filter(|(m, _)| *m == '+').count()
    }

    pub fn removed(&self) -> usize {
        self.lines.iter().filter(|(m, _)| *m == '-').count()
    }

    /// The lines that changed, as `(first, last)` numbers of the new file, and whether there are only removed lines (then both numbers are the line after which they were taken out).
    pub fn changed_range(&self) -> (u32, u32, bool) {
        let mut new_no = self.new_start;
        let (mut first, mut last) = (None::<u32>, 0u32);
        let mut removed_at = None::<u32>;
        for (mark, _) in &self.lines {
            match mark {
                '+' => {
                    first.get_or_insert(new_no);
                    last = new_no;
                    new_no += 1;
                }
                '-' => {
                    // After the line before them; git gives that line itself as the start of a new file range that is empty.
                    removed_at.get_or_insert(if self.new_len == 0 { self.new_start } else { new_no.saturating_sub(1) }.max(1));
                }
                _ => new_no += 1,
            }
        }
        match first {
            Some(f) => (f, last, false),
            None => {
                let at = removed_at.unwrap_or(self.new_start.max(1));
                (at, at, true)
            }
        }
    }
}

/// `@@ -40,3 +40,5 @@ fn load() {` as numbers and the text after it.
fn parse_hunk_header(line: &str) -> Option<Hunk> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, context) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |s: &str| -> Option<(u32, u32)> {
        match s.split_once(',') {
            Some((a, b)) => Some((a.trim().parse().ok()?, b.trim().parse().ok()?)),
            None => Some((s.trim().parse().ok()?, 1)),
        }
    };
    let (old_start, old_len) = range(old)?;
    let (new_start, new_len) = range(new)?;
    Some(Hunk { old_start, old_len, new_start, new_len, context: context.trim().to_string(), lines: Vec::new() })
}

/// The hunks of a patch, in order. The lines before the first `@@` (the `---` and `+++` lines) are not part of any; a hunk ends where the counts of its header say, so that a removed line
/// that begins with `-- ` is not taken for a header, and a patch that was cut ends the last hunk where it ends.
pub fn hunks_of(patch: &str) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = Vec::new();
    let mut open: Option<(Hunk, u32, u32)> = None;
    for line in patch.lines() {
        let line = line.trim_end_matches('\r');
        // A line that starts with `@` is never a line of a hunk (those start with a space, `+` or `-`): it is the header of the next one, whatever the counts of this one said.
        if line.starts_with('@') {
            if let Some(next) = parse_hunk_header(line) {
                if let Some((done, _, _)) = open.take() {
                    out.push(done);
                }
                let (old_len, new_len) = (next.old_len, next.new_len);
                open = Some((next, old_len, new_len));
            }
            continue;
        }
        if let Some((hunk, old_left, new_left)) = open.as_mut() {
            if *old_left > 0 || *new_left > 0 {
                match line.chars().next() {
                    Some('+') => {
                        *new_left = new_left.saturating_sub(1);
                        hunk.lines.push(('+', line[1..].to_string()));
                    }
                    Some('-') => {
                        *old_left = old_left.saturating_sub(1);
                        hunk.lines.push(('-', line[1..].to_string()));
                    }
                    Some(' ') => {
                        *old_left = old_left.saturating_sub(1);
                        *new_left = new_left.saturating_sub(1);
                        hunk.lines.push((' ', line[1..].to_string()));
                    }
                    None => {
                        // A context line that lost its one space.
                        *old_left = old_left.saturating_sub(1);
                        *new_left = new_left.saturating_sub(1);
                        hunk.lines.push((' ', String::new()));
                    }
                    // "\ No newline at end of file", and anything the counts do not account for.
                    _ => {}
                }
            }
        }
    }
    if let Some((done, _, _)) = open {
        out.push(done);
    }
    out
}

/// The files a run changed, from the body of `GET /api/v1/runs/{id}/diff`: the patches of its checkpoint and the files its commands changed (named, with no patch). A file is listed once.
pub fn changes_of(diff: &Value) -> Vec<FileChange> {
    let status_of = |v: &Value| v.as_str().and_then(|s| s.chars().next()).unwrap_or('M');
    let mut out: Vec<FileChange> = Vec::new();
    if let Some(files) = diff["checkpoint"]["files"].as_array() {
        for f in files {
            let Some(path) = f["path"].as_str() else { continue };
            out.push(FileChange {
                path: path.to_string(),
                status: status_of(&f["status"]),
                patch: f["patch"].as_str().unwrap_or("").to_string(),
                truncated: f["truncated"].as_bool().unwrap_or(false),
                binary: f["binary"].as_bool().unwrap_or(false),
            });
        }
    }
    // The checkpoint names more files than it shows a patch for.
    if let Some(paths) = diff["checkpoint"]["paths"].as_array() {
        for p in paths {
            let Some(path) = p["path"].as_str() else { continue };
            if !out.iter().any(|c| c.path == path) {
                out.push(FileChange { path: path.to_string(), status: status_of(&p["status"]), patch: String::new(), truncated: false, binary: false });
            }
        }
    }
    if let Some(commands) = diff["commands"].as_array() {
        for c in commands {
            for f in c["files"].as_array().into_iter().flatten() {
                let Some(path) = f["path"].as_str() else { continue };
                if !out.iter().any(|c| c.path == path) {
                    out.push(FileChange { path: path.to_string(), status: status_of(&f["status"]), patch: String::new(), truncated: false, binary: false });
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// The words of one identifier: `parse_strict`, `parse-strict` and `parseStrict` are "parse strict"; `HTTPServer` is "HTTP Server"; digits stay with the word they follow (`utf8`).
fn words_of_identifier(name: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for part in name.split(|c: char| c == '_' || c == '-' || c == '$' || c.is_whitespace()).filter(|p| !p.is_empty()) {
        let chars: Vec<char> = part.chars().collect();
        let mut current = String::new();
        for (i, &c) in chars.iter().enumerate() {
            if i > 0 && c.is_uppercase() {
                let prev = chars[i - 1];
                let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
                // A hump (parseStrict), or the end of a run of capitals before a word (HTTPServer).
                if prev.is_lowercase() || prev.is_ascii_digit() || (prev.is_uppercase() && next_lower) {
                    words.push(std::mem::take(&mut current));
                }
            }
            current.push(c);
        }
        if !current.is_empty() {
            words.push(current);
        }
    }
    words.retain(|w| !w.is_empty());
    words
}

/// A name in code as it is said: the words it is made of, and "dot" for a `.` or `::` between names. `parse_strict` is "parse strict", `Foo::parseStrict` is "Foo dot parse Strict".
pub fn say_symbol(name: &str) -> String {
    name.split("::").flat_map(|p| p.split('.')).map(|part| words_of_identifier(part).join(" ")).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" dot ")
}

/// The end of a file name as it is said: letters for the short ones that nobody says as a word, the word itself for the others.
fn say_extension(ext: &str) -> String {
    let lower = ext.to_ascii_lowercase();
    let letters = |s: &str| s.chars().map(|c| c.to_ascii_uppercase().to_string()).collect::<Vec<_>>().join(" ");
    match lower.as_str() {
        "rs" | "py" | "ts" | "tsx" | "js" | "jsx" | "md" | "html" | "css" | "c" | "h" | "cpp" | "hpp" | "sh" | "ps1" | "sql" | "php" | "rb" | "kt" | "cc" | "vue" => letters(&lower),
        "cs" => "C sharp".to_string(),
        "yml" | "yaml" => "yaml".to_string(),
        "txt" => "text".to_string(),
        _ if lower.chars().count() <= 2 => letters(&lower),
        _ => lower,
    }
}

/// A file name as it is said: "pipeline.rs" is "pipeline dot R S", ".gitignore" is "dot gitignore", "foo.test.ts" is "foo dot test dot T S".
pub fn say_file_name(name: &str) -> String {
    let name = name.trim();
    let mut parts = name.split('.');
    let stem = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    let mut spoken: Vec<String> = Vec::new();
    if stem.is_empty() {
        spoken.push("dot".into());
    } else {
        spoken.push(say_symbol(stem));
    }
    for (i, ext) in rest.iter().enumerate() {
        if ext.is_empty() {
            continue;
        }
        let last = i + 1 == rest.len();
        if !stem.is_empty() || i > 0 {
            spoken.push("dot".into());
        }
        // Only the end of the name is an extension; "test" in "foo.test.ts" is a word.
        spoken.push(if last && !stem.is_empty() { say_extension(ext) } else { say_symbol(ext) });
    }
    spoken.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ")
}

/// The last part of a path (either kind of separator).
pub fn file_name_of(path: &str) -> &str {
    path.rsplit(['/', '\\']).find(|p| !p.is_empty()).unwrap_or(path)
}

/// A path as it is said, from its last three parts: "core/src/api_gateway/server.rs" is "src slash api gateway slash server dot R S". The drive, `.` and `..` are left out.
pub fn say_path(path: &str) -> String {
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|p| !p.is_empty() && *p != "." && *p != ".." && !(p.len() == 2 && p.ends_with(':'))).collect();
    let Some((file, dirs)) = parts.split_last() else { return String::new() };
    let keep = dirs.len().min(PATH_PARTS - 1);
    let mut spoken: Vec<String> = dirs[dirs.len() - keep..].iter().map(|d| say_symbol(d)).filter(|d| !d.is_empty()).collect();
    spoken.push(say_file_name(file));
    spoken.join(" slash ")
}

/// The name of a mark of punctuation or an operator, longest first.
const OPERATORS: &[(&str, &str)] = &[
    ("<<=", "left shift equals"),
    (">>=", "right shift equals"),
    ("...", "dot dot dot"),
    ("===", "triple equals"),
    ("!==", "not double equals"),
    ("::", "colon colon"),
    ("->", "arrow"),
    ("=>", "fat arrow"),
    ("==", "double equals"),
    ("!=", "not equals"),
    ("<=", "less or equal"),
    (">=", "greater or equal"),
    ("&&", "and and"),
    ("||", "or or"),
    ("++", "plus plus"),
    ("--", "minus minus"),
    ("+=", "plus equals"),
    ("-=", "minus equals"),
    ("*=", "times equals"),
    ("/=", "divide equals"),
    ("..", "dot dot"),
    ("<<", "left shift"),
    (">>", "right shift"),
    ("//", "slash slash"),
    ("(", "open paren"),
    (")", "close paren"),
    ("{", "open brace"),
    ("}", "close brace"),
    ("[", "open bracket"),
    ("]", "close bracket"),
    ("<", "less than"),
    (">", "greater than"),
    ("=", "equals"),
    (":", "colon"),
    (";", "semicolon"),
    (",", "comma"),
    (".", "dot"),
    ("+", "plus"),
    ("-", "minus"),
    ("*", "star"),
    ("/", "slash"),
    ("\\", "backslash"),
    ("&", "ampersand"),
    ("|", "pipe"),
    ("!", "bang"),
    ("?", "question mark"),
    ("#", "hash"),
    ("@", "at"),
    ("$", "dollar"),
    ("%", "percent"),
    ("^", "caret"),
    ("~", "tilde"),
    ("`", "backtick"),
    ("'", "single quote"),
    ("\"", "double quote"),
];

/// A line of code as it is said, in the line-by-line mode: names as the words they are, numbers as they are, punctuation by name, white space not at all.
/// `let x = parse_strict(a);` is "let x equals parse strict open paren a close paren semicolon". A line is cut at [`MAX_SPOKEN_LINE`] characters of what is said.
pub fn say_code(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut said = 0usize;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let piece = if c.is_alphanumeric() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            // A run of digits is a number, said as it is; a name is said as its words.
            if word.chars().all(|c| c.is_ascii_digit()) {
                word
            } else {
                words_of_identifier(&word).join(" ")
            }
        } else {
            let rest: String = chars[i..chars.len().min(i + 3)].iter().collect();
            match OPERATORS.iter().find(|(op, _)| rest.starts_with(op)) {
                Some((op, name)) => {
                    i += op.chars().count();
                    (*name).to_string()
                }
                None => {
                    i += 1;
                    c.to_string()
                }
            }
        };
        if piece.is_empty() {
            continue;
        }
        said += piece.chars().count() + 1;
        out.push(piece);
        if said > MAX_SPOKEN_LINE {
            out.push("and so on".into());
            break;
        }
    }
    out.join(" ")
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// Sentences
// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------

fn lines_word(n: usize) -> String {
    if n == 1 {
        "1 line".to_string()
    } else {
        format!("{n} lines")
    }
}

/// The function, class or module the header of a hunk names, as "function load": the first of `fn`, `def`, `function`, `func`, `class`, `struct`, `enum`, `trait`, `interface`, `impl`, `mod`, `type`
/// with a name after it; a header that has only a call (`load(self):`) is a function.
fn place_of(context: &str) -> Option<String> {
    const KINDS: &[(&str, &str)] = &[
        ("fn", "function"),
        ("def", "function"),
        ("function", "function"),
        ("func", "function"),
        ("class", "class"),
        ("struct", "struct"),
        ("enum", "enum"),
        ("trait", "trait"),
        ("interface", "interface"),
        ("impl", "implementation of"),
        ("mod", "module"),
        ("type", "type"),
    ];
    let name_after = |at: usize| -> Option<String> {
        let name: String = context[at..].trim_start().chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$').collect();
        (!name.is_empty()).then_some(name)
    };
    // Whole words only, the first that has a name after it.
    let mut best: Option<(usize, String)> = None;
    for (kw, word) in KINDS {
        let mut from = 0;
        while let Some(found) = context[from..].find(kw) {
            let at = from + found;
            let end = at + kw.len();
            let before_ok = at == 0 || !context[..at].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_');
            let after_ok = context[end..].chars().next().is_some_and(|c| c.is_whitespace());
            if before_ok && after_ok {
                if let Some(name) = name_after(end) {
                    if best.as_ref().is_none_or(|(b, _)| at < *b) {
                        best = Some((at, format!("{word} {}", say_symbol(&name))));
                    }
                    break;
                }
            }
            from = end;
        }
    }
    if let Some((_, place)) = best {
        return Some(place);
    }
    // No keyword: a name followed by "(" is a function.
    let open = context.find('(')?;
    let name: String = context[..open].trim_end().chars().rev().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$').collect::<Vec<_>>().into_iter().rev().collect();
    (!name.is_empty()).then(|| format!("function {}", say_symbol(&name)))
}

/// The parts of a line of code that a swap compares: names and numbers whole, every other mark alone.
fn tokens(line: &str) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_alphanumeric() || c == '_' || c == '$' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                i += 1;
            }
            out.push(chars[start..i].iter().collect());
        } else {
            out.push(c.to_string());
            i += 1;
        }
    }
    out
}

/// When every removed line differs from the added line that takes its place in one name or number, and it is the same swap in all of them: (the old, the new, whether the old was called).
fn swap_of(removed: &[&str], added: &[&str]) -> Option<(String, String, bool)> {
    if removed.len() != added.len() || removed.is_empty() {
        return None;
    }
    let mut found: Option<(String, String, bool)> = None;
    for (old_line, new_line) in removed.iter().zip(added) {
        let (a, b) = (tokens(old_line), tokens(new_line));
        if a.len() != b.len() {
            return None;
        }
        let differing: Vec<usize> = (0..a.len()).filter(|&i| a[i] != b[i]).collect();
        let [at] = differing[..] else { return None };
        let is_word = |t: &str| t.chars().next().is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$');
        if !is_word(&a[at]) || !is_word(&b[at]) {
            return None;
        }
        let called = a.get(at + 1).is_some_and(|t| t == "(");
        let this = (a[at].clone(), b[at].clone(), called);
        match &found {
            Some(first) if *first != this => return None,
            Some(_) => {}
            None => found = Some(this),
        }
    }
    found
}

/// What a hunk did, in a few words: "replaced the call to parse with parse strict", "added 2 lines", "removed 1 line", "changed 3 lines", "replaced 2 lines with 5 lines".
fn what_of(h: &Hunk) -> String {
    let removed: Vec<&str> = h.lines.iter().filter(|(m, _)| *m == '-').map(|(_, l)| l.as_str()).collect();
    let added: Vec<&str> = h.lines.iter().filter(|(m, _)| *m == '+').map(|(_, l)| l.as_str()).collect();
    match (removed.len(), added.len()) {
        (0, 0) => "changed nothing that shows".to_string(),
        (0, a) => format!("added {}", lines_word(a)),
        (r, 0) => format!("removed {}", lines_word(r)),
        (r, a) => match swap_of(&removed, &added) {
            Some((old, new, called)) => format!("replaced {}{} with {}", if called { "the call to " } else { "" }, say_symbol(&old), say_symbol(&new)),
            None if r == a => format!("changed {}", lines_word(r)),
            None => format!("replaced {} with {}", lines_word(r), lines_word(a)),
        },
    }
}

/// One change in words: "replaced the call to parse with parse strict in function load, lines 40 to 52".
pub fn describe_hunk(h: &Hunk) -> String {
    let (first, last, only_removed) = h.changed_range();
    let place = place_of(&h.context).map(|p| format!(" in {p}")).unwrap_or_default();
    let at = if only_removed {
        format!("after line {first}")
    } else if first == last {
        format!("line {first}")
    } else {
        format!("lines {first} to {last}")
    };
    format!("{}{place}, {at}", what_of(h))
}

/// One file's changes in words: "main.rs: replaced the call to parse with parse strict in function load, lines 40 to 52; added 2 lines in function save, lines 90 to 91."
pub fn describe_file(c: &FileChange) -> String {
    let name = say_file_name(file_name_of(&c.path));
    if c.binary {
        return format!("{name}: a binary file {}.", if c.status == 'A' { "was added" } else if c.status == 'D' { "was deleted" } else { "changed" });
    }
    let hunks = hunks_of(&c.patch);
    let (added, removed) = (hunks.iter().map(Hunk::added).sum::<usize>(), hunks.iter().map(Hunk::removed).sum::<usize>());
    let mut sentence = match (c.status, hunks.is_empty()) {
        ('A', false) => format!("{name}: a new file of {}", lines_word(added)),
        ('D', false) => format!("{name}: deleted, {} gone", lines_word(removed)),
        ('A', true) => format!("{name}: a new file"),
        ('D', true) => format!("{name}: deleted"),
        (_, true) => format!("{name}: changed"),
        (_, false) => {
            let spoken: Vec<String> = hunks.iter().take(MAX_HUNKS_PER_FILE).map(describe_hunk).collect();
            let more = hunks.len().saturating_sub(MAX_HUNKS_PER_FILE);
            let tail = if more > 0 { format!("; and {more} more {}", if more == 1 { "change" } else { "changes" }) } else { String::new() };
            format!("{name}: {}{tail}", spoken.join("; "))
        }
    };
    if c.truncated {
        sentence.push_str(" (the rest of its patch is cut)");
    }
    sentence.push('.');
    sentence
}

/// What was said when there is nothing to read.
pub const NOTHING: &str = "I have no record of any file that run changed.";

/// The whole change in one or two sentences: how many files, how many lines, and the names of the first few. A large change is only this, with a hint to ask for one file.
pub fn summarise(changes: &[FileChange]) -> String {
    if changes.is_empty() {
        return NOTHING.to_string();
    }
    let mut added = 0usize;
    let mut removed = 0usize;
    for c in changes {
        for h in hunks_of(&c.patch) {
            added += h.added();
            removed += h.removed();
        }
    }
    let n = changes.len();
    let head = format!("{n} {} changed", if n == 1 { "file" } else { "files" });
    let counts = if added + removed > 0 { format!(", {} added and {removed} removed", lines_word(added)) } else { String::new() };
    let names: Vec<String> = changes.iter().take(MAX_FILES_NAMED).map(|c| say_file_name(file_name_of(&c.path))).collect();
    let more = n.saturating_sub(MAX_FILES_NAMED);
    let list = match (names.len(), more) {
        (1, _) => names[0].clone(),
        (_, 0) => format!("{} and {}", names[..names.len() - 1].join(", "), names[names.len() - 1]),
        _ => format!("{}, and {more} more", names.join(", ")),
    };
    let big = added + removed > BIG_DIFF_LINES || n > BIG_DIFF_FILES;
    if big {
        format!("{head}{counts}. That is a lot to read out: {list}. Ask for one file, or say next change.")
    } else {
        format!("{head}{counts}: {list}.")
    }
}

/// One change of the run, as "next change" and "previous change" go through them: the sentence, the file, and the hunk when there is one (to be spelled line by line on request).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub path: String,
    pub sentence: String,
    pub hunk: Option<Hunk>,
}

/// One item for every change of the run, file by file, in order. A file that has no patch (a command changed it, or it is binary) is one item that says its name and has no hunk.
/// The list is cut at [`MAX_ITEMS`] items.
pub fn items(changes: &[FileChange]) -> Vec<Item> {
    let mut out = Vec::new();
    'files: for c in changes {
        let hunks = hunks_of(&c.patch);
        let name = say_file_name(file_name_of(&c.path));
        if hunks.is_empty() || c.binary {
            out.push(Item { path: c.path.clone(), sentence: describe_file(c), hunk: None });
        } else {
            for h in hunks {
                out.push(Item { path: c.path.clone(), sentence: format!("{name}: {}.", describe_hunk(&h)), hunk: Some(h) });
                if out.len() >= MAX_ITEMS {
                    break 'files;
                }
            }
        }
        if out.len() >= MAX_ITEMS {
            break;
        }
    }
    out
}

/// How many changes the person can go through one by one.
pub const MAX_ITEMS: usize = 200;

/// The line-by-line mode for one change: every changed line with its number, at most [`MAX_SPELLED_LINES`] of them. (A hunk is spelled from the text it holds, which the diff route masked.) "line 41 removed: parse open paren a close paren semicolon.
/// line 41 added: parse strict open paren a close paren semicolon."
pub fn spell_hunk(h: &Hunk) -> String {
    let (mut old_no, mut new_no) = (h.old_start, h.new_start);
    let mut said: Vec<String> = Vec::new();
    let mut changed = 0usize;
    for (mark, text) in &h.lines {
        match mark {
            '-' => {
                if changed < MAX_SPELLED_LINES {
                    said.push(format!("line {old_no} removed: {}.", say_code(text)));
                }
                changed += 1;
                old_no += 1;
            }
            '+' => {
                if changed < MAX_SPELLED_LINES {
                    said.push(format!("line {new_no} added: {}.", say_code(text)));
                }
                changed += 1;
                new_no += 1;
            }
            _ => {
                old_no += 1;
                new_no += 1;
            }
        }
    }
    if changed > MAX_SPELLED_LINES {
        said.push(format!("And {} more changed lines.", changed - MAX_SPELLED_LINES));
    }
    said.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PATCH: &str = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -38,7 +38,7 @@ fn load(path: &str) -> Config {\n     let text = read(path);\n     let n = 1;\n-    let cfg = parse(&text);\n+    let cfg = parse_strict(&text);\n     check(&cfg);\n     cfg\n }\n@@ -90,3 +90,5 @@ fn save(cfg: &Config) {\n     write(cfg);\n+    log(\"saved\");\n+    flush();\n }";

    fn change(path: &str, patch: &str) -> FileChange {
        FileChange { path: path.into(), status: 'M', patch: patch.into(), truncated: false, binary: false }
    }

    #[test]
    fn hunks_are_read_with_their_numbers_their_place_and_their_lines() {
        let hunks = hunks_of(PATCH);
        assert_eq!(hunks.len(), 2);
        assert_eq!((hunks[0].old_start, hunks[0].old_len, hunks[0].new_start, hunks[0].new_len), (38, 7, 38, 7));
        assert_eq!(hunks[0].context, "fn load(path: &str) -> Config {");
        assert_eq!((hunks[0].added(), hunks[0].removed()), (1, 1));
        assert_eq!((hunks[1].added(), hunks[1].removed()), (2, 0));
        // The lines that changed, not the lines around them: the third line of the first hunk is line 40.
        assert_eq!(hunks[0].changed_range(), (40, 40, false));
        assert_eq!(hunks[1].changed_range(), (91, 92, false));
        // A header with no count is a count of one.
        let one = hunks_of("--- a/x\n+++ b/x\n@@ -5 +5 @@\n-a\n+b");
        assert_eq!((one[0].old_len, one[0].new_len, one[0].lines.len()), (1, 1, 2));
        assert!(hunks_of("").is_empty() && hunks_of("Binary file differs").is_empty());
    }

    #[test]
    fn a_removed_line_that_looks_like_a_header_is_still_a_removed_line() {
        // `-- comment` removed is written `--- comment`; and `++ x` added is `+++ x`: the counts of the header say they are lines of the hunk.
        let patch = "--- a/q.sql\n+++ b/q.sql\n@@ -1,3 +1,3 @@\n context\n--- comment\n+-- other comment\n tail\n--- a/z.sql\n+++ b/z.sql\n@@ -1 +1 @@\n-x\n+y";
        let hunks = hunks_of(patch);
        assert_eq!(hunks.len(), 2, "{hunks:?}");
        assert_eq!((hunks[0].removed(), hunks[0].added()), (1, 1));
        assert_eq!(hunks[0].lines[1], ('-', "-- comment".to_string()));
    }

    #[test]
    fn a_change_is_described_by_its_swap_its_place_and_its_lines() {
        let hunks = hunks_of(PATCH);
        assert_eq!(describe_hunk(&hunks[0]), "replaced the call to parse with parse strict in function load, line 40");
        assert_eq!(describe_hunk(&hunks[1]), "added 2 lines in function save, lines 91 to 92");
        let file = describe_file(&change("core/src/main.rs", PATCH));
        assert_eq!(file, "main dot R S: replaced the call to parse with parse strict in function load, line 40; added 2 lines in function save, lines 91 to 92.");
    }

    #[test]
    fn other_kinds_of_change_are_said_in_their_own_words() {
        let hunk = |body: &str| hunks_of(&format!("--- a/f\n+++ b/f\n{body}")).remove(0);
        assert_eq!(what_of(&hunk("@@ -1,2 +1 @@\n-one\n-two")), "removed 2 lines");
        assert_eq!(what_of(&hunk("@@ -1 +1,1 @@\n-width = 5\n+width = 10")), "replaced 5 with 10");
        assert_eq!(what_of(&hunk("@@ -1 +1 @@\n-let a = 1;\n+let b = 2;")), "changed 1 line", "two swaps in one line are not a swap");
        assert_eq!(what_of(&hunk("@@ -1,2 +1,3 @@\n-a\n-b\n+c\n+d\n+e")), "replaced 2 lines with 3 lines");
        assert_eq!(what_of(&hunk("@@ -1,2 +1,2 @@\n-foo(1)\n-foo(2)\n+bar(1)\n+bar(2)")), "replaced the call to foo with bar", "the same swap in every line");
        assert_eq!(what_of(&hunk("@@ -1,2 +1,2 @@\n-foo(1)\n-foo(2)\n+bar(1)\n+baz(2)")), "changed 2 lines", "different swaps are not one");
        // Only removed lines: after the line before them.
        assert_eq!(describe_hunk(&hunk("@@ -10,3 +9,1 @@\n keep\n-gone\n-gone too")), "removed 2 lines, after line 9");
        assert_eq!(describe_hunk(&hunk("@@ -5,2 +4,0 @@\n-gone\n-gone too")), "removed 2 lines, after line 4", "an empty range of the new file is given by the line before it");
        // No place when the header names none; a class and an impl.
        assert_eq!(describe_hunk(&hunk("@@ -1 +1,2 @@\n x\n+y")), "added 1 line, line 2");
        assert_eq!(place_of("class ParseError(Exception):").as_deref(), Some("class Parse Error"));
        assert_eq!(place_of("impl Store {").as_deref(), Some("implementation of Store"));
        assert_eq!(place_of("pub async fn load_all(&self)").as_deref(), Some("function load all"));
        assert_eq!(place_of("def load(self):").as_deref(), Some("function load"));
        assert_eq!(place_of("    parseStrict(text) {").as_deref(), Some("function parse Strict"));
        assert_eq!(place_of("// just a comment"), None);
        assert_eq!(place_of("let functional = 1;"), None, "a word that only starts with a keyword is not one");
    }

    #[test]
    fn a_summary_says_the_number_of_files_and_lines_and_names_the_first_few() {
        let one = summarise(&[change("src/main.rs", PATCH)]);
        assert_eq!(one, "1 file changed, 3 lines added and 1 removed: main dot R S.");
        let two = summarise(&[change("src/main.rs", PATCH), change("lib/util.py", "--- a/u\n+++ b/u\n@@ -1 +1 @@\n-a\n+b")]);
        assert_eq!(two, "2 files changed, 4 lines added and 2 removed: main dot R S and util dot P Y.");
        assert_eq!(summarise(&[]), NOTHING);
        // A file that only a command changed has no patch: named, no counts.
        assert_eq!(summarise(&[change("notes.txt", "")]), "1 file changed: notes dot text.");
        // Many files: the first six by name, the rest counted, and a hint.
        let many: Vec<FileChange> = (0..10).map(|i| change(&format!("src/file_{i}.rs"), "")).collect();
        let s = summarise(&many);
        assert!(s.starts_with("10 files changed. That is a lot to read out: "), "{s}");
        assert!(s.contains("and 4 more") && s.contains("file 0 dot R S") && !s.contains("file 6"), "{s}");
        // Many lines.
        let body: String = (0..450).map(|i| format!("+line {i}\n")).collect();
        let big = change("big.txt", &format!("--- a/big.txt\n+++ b/big.txt\n@@ -1,0 +1,450 @@\n{body}"));
        assert!(summarise(&[big]).contains("That is a lot to read out"));
    }

    #[test]
    fn items_are_one_sentence_for_every_change_and_new_and_deleted_files_and_binaries_are_said() {
        let list = items(&[change("src/main.rs", PATCH), FileChange { path: "img.png".into(), status: 'M', patch: "Binary file differs".into(), truncated: false, binary: true }]);
        assert_eq!(list.len(), 3, "{list:?}");
        assert!(list[0].sentence.starts_with("main dot R S: replaced the call to parse") && list[2].sentence == "img dot png: a binary file changed.", "{list:?}");
        assert!(list[0].hunk.is_some() && list[2].hunk.is_none(), "a change of text can be spelled, a binary file cannot");
        let added = FileChange { path: "new.rs".into(), status: 'A', patch: "--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1,3 @@\n+a\n+b\n+c".into(), truncated: false, binary: false };
        assert_eq!(describe_file(&added), "new dot R S: a new file of 3 lines.");
        let deleted = FileChange { path: "old.rs".into(), status: 'D', patch: "--- a/old.rs\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-a\n-b".into(), truncated: true, binary: false };
        assert_eq!(describe_file(&deleted), "old dot R S: deleted, 2 lines gone (the rest of its patch is cut).");
    }

    #[test]
    fn many_changes_in_a_file_are_counted_after_the_first_six() {
        let mut patch = String::from("--- a/f.rs\n+++ b/f.rs\n");
        for i in 0..9 {
            patch.push_str(&format!("@@ -{n},1 +{n},1 @@\n-a{i}\n+b{i}\n", n = 10 * (i + 1)));
        }
        let spoken = describe_file(&change("f.rs", &patch));
        assert!(spoken.ends_with("; and 3 more changes."), "{spoken}");
        assert_eq!(items(&[change("f.rs", &patch)]).len(), 9, "the list the person goes through has all of them");
    }

    #[test]
    fn symbols_paths_and_files_are_said_the_way_a_person_says_them() {
        assert_eq!(say_symbol("parse_strict"), "parse strict");
        assert_eq!(say_symbol("parseStrict"), "parse Strict");
        assert_eq!(say_symbol("HTTPServer"), "HTTP Server");
        assert_eq!(say_symbol("utf8_len"), "utf8 len");
        assert_eq!(say_symbol("Foo::bar_baz"), "Foo dot bar baz");
        assert_eq!(say_symbol("self.load"), "self dot load");
        assert_eq!(say_symbol(""), "");
        assert_eq!(say_file_name("pipeline.rs"), "pipeline dot R S");
        assert_eq!(say_file_name("foo.test.ts"), "foo dot test dot T S");
        assert_eq!(say_file_name(".gitignore"), "dot gitignore");
        assert_eq!(say_file_name("Cargo.toml"), "Cargo dot toml");
        assert_eq!(say_file_name("README.md"), "README dot M D");
        assert_eq!(say_file_name("Makefile"), "Makefile");
        assert_eq!(say_file_name("api_gateway.yml"), "api gateway dot yaml");
        assert_eq!(say_path("core/src/api_gateway/server.rs"), "src slash api gateway slash server dot R S");
        assert_eq!(say_path("src\\main.rs"), "src slash main dot R S");
        assert_eq!(say_path("C:\\Users\\me\\proj\\lib.rs"), "me slash proj slash lib dot R S");
        assert_eq!(say_path("./main.rs"), "main dot R S");
        assert_eq!(say_path(""), "");
        assert_eq!(file_name_of("a/b\\c.rs"), "c.rs");
    }

    #[test]
    fn a_line_of_code_is_said_by_the_names_in_it_and_the_marks_by_their_names() {
        assert_eq!(say_code("let cfg = parse_strict(&text);"), "let cfg equals parse strict open paren ampersand text close paren semicolon");
        assert_eq!(say_code("if a == b && c != 0 {"), "if a double equals b and and c not equals 0 open brace");
        assert_eq!(say_code("x => y.z"), "x fat arrow y dot z");
        assert_eq!(say_code("fn load() -> Config {"), "fn load open paren close paren arrow Config open brace");
        assert_eq!(say_code("Foo::new()"), "Foo colon colon new open paren close paren");
        assert_eq!(say_code("   \t "), "");
        let long = say_code(&"word ".repeat(200));
        assert!(long.ends_with("and so on") && long.chars().count() < MAX_SPOKEN_LINE + 40, "{long}");
    }

    #[test]
    fn the_line_by_line_mode_says_every_changed_line_with_its_number_and_cuts_a_long_change() {
        let hunks = hunks_of(PATCH);
        assert_eq!(spell_hunk(&hunks[0]), "line 40 removed: let cfg equals parse open paren ampersand text close paren semicolon. line 40 added: let cfg equals parse strict open paren ampersand text close paren semicolon.");
        let body: String = (0..40).map(|i| format!("+let v{i} = {i};\n")).collect();
        let big = hunks_of(&format!("--- a/f\n+++ b/f\n@@ -1,0 +1,40 @@\n{body}"));
        let spelled = spell_hunk(&big[0]);
        assert!(spelled.ends_with("And 10 more changed lines.") && spelled.matches("added:").count() == MAX_SPELLED_LINES, "{spelled}");
    }

    #[test]
    fn the_changes_of_a_run_are_read_from_the_body_of_the_diff_route() {
        let body = json!({
            "run_id": "r1",
            "checkpoint": {
                "root": "D:/proj",
                "changed": 3,
                "paths": [{"path": "src/main.rs", "status": "M"}, {"path": "src/extra.rs", "status": "A"}],
                "files": [{"path": "src/main.rs", "status": "M", "patch": PATCH, "truncated": false, "binary": false}],
                "more_files": 0
            },
            "commands": [{"call_id": 4, "command": "cargo fmt", "changed": 2, "files": [{"path": "src/main.rs", "status": "M"}, {"path": "src/fmt.rs", "status": "M"}], "error": null}],
            "error": null
        });
        let changes = changes_of(&body);
        let names: Vec<(&str, char, bool)> = changes.iter().map(|c| (c.path.as_str(), c.status, c.patch.is_empty())).collect();
        assert_eq!(names, [("src/main.rs", 'M', false), ("src/extra.rs", 'A', true), ("src/fmt.rs", 'M', true)], "a file is listed once, with its patch when there is one");
        assert!(changes_of(&json!({"checkpoint": null, "commands": []})).is_empty());
        assert!(changes_of(&json!(null)).is_empty());
    }
}

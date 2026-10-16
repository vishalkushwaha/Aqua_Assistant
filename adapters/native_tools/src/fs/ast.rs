//! The syntax gate of the file tools (card OA6-03; tasks ED-09 and CU-14 of the OpenCode audit).
//!
//! This file was a placeholder: `validate_patch_syntax` constructed a tree-sitter parser and always answered `Ok`. No grammar crate is a dependency of this build and none was added
//! (decision D-06, option B of the audit), so the gate is made of what is in process:
//!
//! * **JSON**: `serde_json` parses the text before and after a change. A change that turns a JSON file that parsed into one that does not is **refused** (with `AQUA_SYNTAX_GATE=on`):
//!   nothing is written and the answer says the line. A file that was already broken is never blocked, and the files that may hold comments (`tsconfig.json`, `.vscode/*.json`, `*.jsonc`,
//!   `*.json5`, ...) are not parsed as JSON.
//! * **Brackets and quotes**: for Rust, JavaScript, TypeScript, Go, C and C++, Java, C#, Kotlin, Python, TOML, CSS and the JSON that holds comments, a scanner that knows each language's
//!   strings and comments counts the brackets and quotes before and after. It only **warns** (a note after the change, whatever the setting): a text scanner is a guess, and a guess never
//!   stops a write. It says something only when the change leaves a problem the file did not have (a `{` never closed, a `)` that closes nothing, a string or a comment that never ends).
//! * TOML has no parser here (the `toml` crate is not in `Cargo.lock`), so it gets the scanner and one more note: a line that is no `key = value`, no table header and no comment.
//! * Everything else (Markdown, YAML, HTML, shell, a file over 1 MiB, an unknown extension) passes without a word. The compiler's own complaints after an edit are read by
//!   `check::diagnostics`, which runs nothing either.
//!
//! The setting is [`Gate`]: off (nothing is looked at, as before), `warn` (notes only) and `on` (a JSON change that breaks the file is refused). New behaviour ships off.
//!
//! The `tree-sitter` line of `Cargo.toml` is still there only because removing it would change `Cargo.lock`; nothing here uses it (listed for the owner in the handoff of the card).

use serde::de::IgnoredAny;

/// What the person set (`AQUA_SYNTAX_GATE`): nothing looked at, notes only, or notes and the refusal of a JSON change that breaks the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Gate {
    #[default]
    Off,
    Warn,
    On,
}

impl Gate {
    /// The setting a word names (`on`, `warn`); anything else is off.
    pub fn parse(word: &str) -> Gate {
        match word.trim().to_ascii_lowercase().as_str() {
            "on" | "1" | "true" => Gate::On,
            "warn" => Gate::Warn,
            _ => Gate::Off,
        }
    }
}

/// What the gate says about one change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to say.
    Fine,
    /// The change is made, and the model is told this after it.
    Note(String),
    /// The change is not made. One sentence: the line, and what is wrong.
    Refuse(String),
}

/// The largest text that is looked at (before or after): a bigger file is a generated one, and a scan of it is no help.
pub const MAX_BYTES: usize = 1 << 20;

/// The most problems a scan keeps.
const MAX_PROBLEMS: usize = 40;

/// What the gate says about a change of the file called `name` (a path, as the tools show it) from `old` to `new`. `old` is empty for a file that is made. With the gate off, for an
/// unchanged text, for a text over [`MAX_BYTES`] and for a name that has no check, it is [`Verdict::Fine`].
pub fn check_change(gate: Gate, name: &str, old: &str, new: &str) -> Verdict {
    if gate == Gate::Off || old == new || old.len() > MAX_BYTES || new.len() > MAX_BYTES {
        return Verdict::Fine;
    }
    let (file, kind) = kind_of(name);
    match kind {
        Kind::Json => match json_broken(old, new) {
            Some((line, what)) if gate == Gate::On => Verdict::Refuse(refusal(line, &what)),
            Some((line, what)) => Verdict::Note(format!("{file}: syntax error introduced at line {line}: the file is now invalid JSON ({what}). It was written as given; read it again and correct line {line}.")),
            None => Verdict::Fine,
        },
        Kind::Scan(lang) => match new_problem(&scan(old, lang), &scan(new, lang)) {
            Some(p) => Verdict::Note(format!("{file}: {}. The file was written as given; check the brackets and quotes around that line.", p.describe())),
            None => Verdict::Fine,
        },
        Kind::None => Verdict::Fine,
    }
}

fn refusal(line: usize, what: &str) -> String {
    format!("syntax error introduced at line {line}: this change would leave the file as invalid JSON ({what}). Correct the JSON in your change and send it again.")
}

// ---- which files are looked at ---------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lang {
    Rust,
    /// JavaScript, JSX and TSX: a `'` can be an apostrophe in the text of a tag, so a quote that is not closed on its line is no string here.
    Js,
    Ts,
    Go,
    C,
    Cpp,
    Java,
    CSharp,
    Kotlin,
    Python,
    Toml,
    Css,
    /// JSON that may hold comments.
    Jsonc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Json,
    Scan(Lang),
    None,
}

/// The file name (the last part of `name`) and what the gate does with a file of that name.
fn kind_of(name: &str) -> (String, Kind) {
    let slashed = name.replace('\\', "/");
    let lower = slashed.to_ascii_lowercase();
    let file = lower.rsplit('/').next().unwrap_or(&lower).to_string();
    let shown = slashed.rsplit('/').next().unwrap_or(&slashed).to_string();
    let Some((stem, ext)) = file.rsplit_once('.') else { return (shown, Kind::None) };
    let kind = match ext {
        "json" => {
            // The JSON that holds comments by its name: a comment there is not a mistake.
            let with_comments = stem.starts_with("tsconfig") || stem.starts_with("jsconfig") || stem.starts_with(".eslintrc") || stem == ".babelrc" || stem == "devcontainer" || stem == ".devcontainer" || format!("/{lower}").contains("/.vscode/");
            if with_comments {
                Kind::Scan(Lang::Jsonc)
            } else {
                Kind::Json
            }
        }
        "jsonc" | "json5" | "code-workspace" => Kind::Scan(Lang::Jsonc),
        "geojson" | "webmanifest" | "ipynb" => Kind::Json,
        "rs" => Kind::Scan(Lang::Rust),
        "js" | "jsx" | "mjs" | "cjs" | "tsx" => Kind::Scan(Lang::Js),
        "ts" | "mts" | "cts" => Kind::Scan(Lang::Ts),
        "go" => Kind::Scan(Lang::Go),
        "c" | "h" => Kind::Scan(Lang::C),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Kind::Scan(Lang::Cpp),
        "java" => Kind::Scan(Lang::Java),
        "cs" => Kind::Scan(Lang::CSharp),
        "kt" | "kts" => Kind::Scan(Lang::Kotlin),
        "py" | "pyw" => Kind::Scan(Lang::Python),
        "toml" => Kind::Scan(Lang::Toml),
        "css" => Kind::Scan(Lang::Css),
        _ => Kind::None,
    };
    (shown, kind)
}

// ---- JSON ------------------------------------------------------------------------------------------------------------------------

/// `(line, what serde_json says)` for a text that is not JSON, `None` for one that is. A byte order mark is not part of the text.
fn json_error(text: &str) -> Option<(usize, String)> {
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    match serde_json::from_str::<IgnoredAny>(body) {
        Ok(_) => None,
        Err(e) => {
            let shown = e.to_string();
            let what = shown.rsplit_once(" at line ").map_or(shown.as_str(), |(m, _)| m).to_string();
            Some((e.line().max(1), what))
        }
    }
}

/// `(line, what)` for a change that breaks a JSON file that was valid; `None` when the file was not valid before (nothing is blamed on the change), is empty, or still is valid.
fn json_broken(old: &str, new: &str) -> Option<(usize, String)> {
    // A file that does not exist yet, or is empty, has no earlier state to break.
    if old.trim().is_empty() || json_error(old).is_some() {
        return None;
    }
    json_error(new)
}

// ---- the scanner -----------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trouble {
    /// A bracket that is opened and not closed.
    Unclosed,
    /// A bracket that closes nothing.
    Stray,
    /// A bracket that closes another kind.
    Mismatch,
    /// A string that does not end.
    OpenString,
    /// A comment that does not end.
    OpenComment,
    /// TOML: a line that says nothing.
    NotAssignment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Problem {
    trouble: Trouble,
    /// The bracket, or the quote that opens the string.
    ch: char,
    /// For a mismatch, the opener the closer should have matched.
    other: char,
    line: usize,
    other_line: usize,
}

impl Problem {
    fn describe(&self) -> String {
        let c = self.ch;
        match self.trouble {
            Trouble::Unclosed => format!("the `{c}` opened at line {} is never closed", self.line),
            Trouble::Stray => format!("the `{c}` at line {} closes nothing", self.line),
            Trouble::Mismatch => format!("the `{c}` at line {} does not match the `{}` opened at line {}", self.line, self.other, self.other_line),
            Trouble::OpenString => format!("the string that starts at line {} is not closed", self.line),
            Trouble::OpenComment => format!("the comment that starts at line {} is never closed", self.line),
            Trouble::NotAssignment => format!("line {} is no `key = value`, no table header and no comment", self.line),
        }
    }

    fn key(&self) -> (Trouble, char) {
        (self.trouble, self.ch)
    }
}

/// The first problem of `after` that `before` did not have: more of one kind than the text had before.
fn new_problem(before: &[Problem], after: &[Problem]) -> Option<Problem> {
    after.iter().find(|p| after.iter().filter(|q| q.key() == p.key()).count() > before.iter().filter(|q| q.key() == p.key()).count()).cloned()
}

struct Frame {
    ch: char,
    line: usize,
    /// The `${` of a template string.
    hole: bool,
}

struct Scan<'a> {
    c: &'a [char],
    lang: Lang,
    i: usize,
    line: usize,
    stack: Vec<Frame>,
    problems: Vec<Problem>,
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Scans `text` as `lang` and says what is wrong with its brackets and quotes. Never panics, whatever the text is.
fn scan(text: &str, lang: Lang) -> Vec<Problem> {
    if text.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = text.strip_prefix('\u{feff}').unwrap_or(text).chars().filter(|c| *c != '\r').collect();
    let mut s = Scan { c: &chars, lang, i: 0, line: 1, stack: Vec::new(), problems: Vec::new() };
    s.run();
    s.problems
}

impl Scan<'_> {
    /// The character `k` places on (a NUL past the end).
    fn at(&self, k: usize) -> char {
        self.c.get(self.i + k).copied().unwrap_or('\0')
    }

    fn starts(&self, s: &str) -> bool {
        s.chars().enumerate().all(|(k, ch)| self.at(k) == ch)
    }

    fn done(&self) -> bool {
        self.i >= self.c.len()
    }

    fn prev(&self) -> char {
        if self.i == 0 {
            '\0'
        } else {
            self.c[self.i - 1]
        }
    }

    fn push_problem(&mut self, p: Problem) {
        if self.problems.len() < MAX_PROBLEMS {
            self.problems.push(p);
        }
    }

    fn simple(&mut self, trouble: Trouble, ch: char, line: usize) {
        self.push_problem(Problem { trouble, ch, other: ch, line, other_line: line });
    }

    fn line_comment(&self) -> Option<&'static str> {
        match self.lang {
            Lang::Python | Lang::Toml => Some("#"),
            Lang::Css => None,
            _ => Some("//"),
        }
    }

    fn block_comments(&self) -> bool {
        !matches!(self.lang, Lang::Python | Lang::Toml)
    }

    fn run(&mut self) {
        let toml = self.lang == Lang::Toml;
        if toml {
            self.toml_line_check();
        }
        while !self.done() {
            let ch = self.at(0);
            if ch == '\n' {
                self.line += 1;
                self.i += 1;
                if toml && self.stack.is_empty() {
                    self.toml_line_check();
                }
                continue;
            }
            if let Some(lc) = self.line_comment() {
                if self.starts(lc) {
                    while !self.done() && self.at(0) != '\n' {
                        self.i += 1;
                    }
                    continue;
                }
            }
            if self.block_comments() && self.starts("/*") {
                self.block_comment();
                continue;
            }
            if self.string_start() {
                continue;
            }
            match ch {
                '(' | '[' | '{' => {
                    self.stack.push(Frame { ch, line: self.line, hole: false });
                    self.i += 1;
                }
                ')' | ']' | '}' => {
                    let hole = self.close(ch);
                    self.i += 1;
                    // The end of the `${ ... }` of a template string: the string goes on.
                    if hole {
                        self.template_body();
                    }
                }
                _ => self.i += 1,
            }
        }
        // Whatever is still open, the outermost first.
        let left: Vec<(char, usize)> = self.stack.iter().map(|f| (if f.hole { '{' } else { f.ch }, f.line)).collect();
        for (ch, line) in left {
            self.simple(Trouble::Unclosed, ch, line);
        }
    }

    /// A closer. True when it ends the `${` of a template string.
    fn close(&mut self, ch: char) -> bool {
        let want = match ch {
            ')' => '(',
            ']' => '[',
            _ => '{',
        };
        let line = self.line;
        match self.stack.last().map(|f| (f.ch, f.line, f.hole)) {
            None => self.simple(Trouble::Stray, ch, line),
            Some((_, _, true)) if ch == '}' => {
                self.stack.pop();
                return true;
            }
            Some((open, open_line, hole)) if hole || open != want => {
                self.push_problem(Problem { trouble: Trouble::Mismatch, ch, other: if hole { '{' } else { open }, line, other_line: open_line });
            }
            Some(_) => {
                self.stack.pop();
            }
        }
        false
    }

    fn block_comment(&mut self) {
        let start = self.line;
        let nested = self.lang == Lang::Rust;
        let mut depth = 1usize;
        self.i += 2;
        loop {
            if self.done() {
                self.simple(Trouble::OpenComment, '/', start);
                return;
            }
            if self.starts("*/") {
                self.i += 2;
                depth -= 1;
                if depth == 0 {
                    return;
                }
            } else if nested && self.starts("/*") {
                self.i += 2;
                depth += 1;
            } else {
                if self.at(0) == '\n' {
                    self.line += 1;
                }
                self.i += 1;
            }
        }
    }

    /// TOML, at the start of a line outside any bracket or string: the line is a comment, a table header, blank, or has an `=`.
    fn toml_line_check(&mut self) {
        let mut k = 0;
        while matches!(self.at(k), ' ' | '\t' | '\r') {
            k += 1;
        }
        let first = self.at(k);
        if matches!(first, '\n' | '\0' | '#' | '[') {
            return;
        }
        let mut j = k;
        while !matches!(self.at(j), '\n' | '\0') {
            if self.at(j) == '=' {
                return;
            }
            j += 1;
        }
        let line = self.line;
        self.simple(Trouble::NotAssignment, ' ', line);
    }

    /// Does a string or a character literal start here? If it does, it is consumed (and a problem noted if it does not end) and the answer is true.
    fn string_start(&mut self) -> bool {
        let ch = self.at(0);
        match self.lang {
            Lang::Rust => match ch {
                '"' => {
                    self.quoted('"', true, true, false);
                    true
                }
                '\'' => {
                    self.rust_tick();
                    true
                }
                'r' | 'b' if !is_ident(self.prev()) => self.rust_raw(),
                _ => false,
            },
            Lang::Js | Lang::Ts => match ch {
                '`' => {
                    self.i += 1;
                    self.template_body();
                    true
                }
                '"' | '\'' => {
                    let jsx = self.lang == Lang::Js;
                    // The text of a tag has apostrophes and quotes in it (`Aqua's own`, `5" wide`); a quote that follows a letter or a digit starts no string in code that can hold JSX.
                    if jsx && is_ident(self.prev()) {
                        return false;
                    }
                    self.quoted(ch, true, false, jsx);
                    true
                }
                '/' => self.regex_literal(),
                _ => false,
            },
            Lang::Go => match ch {
                '`' => {
                    self.raw_until('`', 1, 0);
                    true
                }
                '"' | '\'' => {
                    self.quoted(ch, true, false, false);
                    true
                }
                _ => false,
            },
            Lang::C | Lang::Cpp => match ch {
                '"' => {
                    if self.lang == Lang::Cpp && self.prev() == 'R' && self.cpp_raw() {
                        return true;
                    }
                    self.quoted('"', true, false, false);
                    true
                }
                '\'' => {
                    // `1'000'000`: a digit separator, not a character.
                    if self.prev().is_ascii_digit() && self.at(1).is_ascii_alphanumeric() {
                        self.i += 1;
                    } else {
                        self.quoted('\'', true, false, false);
                    }
                    true
                }
                _ => false,
            },
            Lang::Java | Lang::Kotlin | Lang::CSharp => match ch {
                '"' => {
                    if self.starts("\"\"\"") {
                        self.triple('"', true);
                    } else if self.lang == Lang::CSharp && (self.prev() == '@' || (self.prev() == '$' && self.i >= 2 && self.c[self.i - 2] == '@')) {
                        self.verbatim();
                    } else {
                        self.quoted('"', true, false, false);
                    }
                    true
                }
                '\'' => {
                    self.quoted('\'', true, false, false);
                    true
                }
                _ => false,
            },
            Lang::Python => match ch {
                '"' | '\'' => {
                    if self.starts(&ch.to_string().repeat(3)) {
                        self.triple(ch, true);
                    } else {
                        self.quoted(ch, true, false, false);
                    }
                    true
                }
                _ => false,
            },
            Lang::Toml => match ch {
                '"' | '\'' => {
                    // A literal string has no escapes.
                    let escapes = ch == '"';
                    if self.starts(&ch.to_string().repeat(3)) {
                        self.triple(ch, escapes);
                    } else {
                        self.quoted(ch, escapes, false, false);
                    }
                    true
                }
                _ => false,
            },
            Lang::Css | Lang::Jsonc => match ch {
                '"' | '\'' => {
                    self.quoted(ch, true, false, false);
                    true
                }
                _ => false,
            },
        }
    }

    /// A string that starts at `q` and ends at the same quote. `multiline`: a line break is part of it. `forgive`: a quote that is not closed on its line is no string (the rest of the line
    /// is read as code), as an apostrophe in the text of a JSX tag.
    fn quoted(&mut self, q: char, escapes: bool, multiline: bool, forgive: bool) {
        let (start_line, start_i) = (self.line, self.i);
        self.i += 1;
        loop {
            if self.done() {
                if forgive {
                    self.i = start_i + 1;
                    self.line = start_line;
                } else {
                    self.simple(Trouble::OpenString, q, start_line);
                }
                return;
            }
            let ch = self.at(0);
            if escapes && ch == '\\' {
                if self.at(1) == '\n' {
                    self.line += 1;
                }
                self.i = (self.i + 2).min(self.c.len());
                continue;
            }
            if ch == q {
                self.i += 1;
                return;
            }
            if ch == '\n' {
                if multiline {
                    self.line += 1;
                    self.i += 1;
                    continue;
                }
                if forgive {
                    self.i = start_i + 1;
                } else {
                    self.simple(Trouble::OpenString, q, start_line);
                }
                // The line break is left for the main loop.
                return;
            }
            self.i += 1;
        }
    }

    /// A string in triple quotes (`"""`, `'''`).
    fn triple(&mut self, q: char, escapes: bool) {
        let start = self.line;
        let end = q.to_string().repeat(3);
        self.i += 3;
        loop {
            if self.done() {
                self.simple(Trouble::OpenString, q, start);
                return;
            }
            if self.starts(&end) {
                self.i += 3;
                return;
            }
            let ch = self.at(0);
            if escapes && ch == '\\' {
                if self.at(1) == '\n' {
                    self.line += 1;
                }
                self.i = (self.i + 2).min(self.c.len());
                continue;
            }
            if ch == '\n' {
                self.line += 1;
            }
            self.i += 1;
        }
    }

    /// A string that has no escapes and ends at `q` followed by `hashes` hash marks (a Rust raw string, a Go raw string). `skip` characters are the opening.
    fn raw_until(&mut self, q: char, skip: usize, hashes: usize) {
        let start = self.line;
        self.i += skip;
        loop {
            if self.done() {
                self.simple(Trouble::OpenString, q, start);
                return;
            }
            if self.at(0) == q && (1..=hashes).all(|k| self.at(k) == '#') {
                self.i += 1 + hashes;
                return;
            }
            if self.at(0) == '\n' {
                self.line += 1;
            }
            self.i += 1;
        }
    }

    /// Rust: `r"..."`, `r#"..."#`, `br"..."`. False when this is no raw string (the `r` is part of a name).
    fn rust_raw(&mut self) -> bool {
        let mut k = 0;
        if self.at(0) == 'b' {
            k = 1;
        }
        if self.at(k) != 'r' {
            return false;
        }
        k += 1;
        let mut hashes = 0;
        while self.at(k) == '#' {
            hashes += 1;
            k += 1;
        }
        if self.at(k) != '"' {
            return false;
        }
        self.raw_until('"', k + 1, hashes);
        true
    }

    /// Rust `'`: a character (`'x'`, `'\n'`, `'\u{1F600}'`) or a lifetime or label (`'a`, `'static`), which is no string.
    fn rust_tick(&mut self) {
        if self.at(1) == '\\' {
            // An escape: the closing tick is within a few characters on the same line.
            let mut k = 2;
            if self.at(k) == 'u' && self.at(k + 1) == '{' {
                while !matches!(self.at(k), '}' | '\n' | '\0') && k < 14 {
                    k += 1;
                }
            }
            k += 1;
            while k < 14 && !matches!(self.at(k), '\'' | '\n' | '\0') {
                k += 1;
            }
            if self.at(k) == '\'' {
                self.i += k + 1;
                return;
            }
            self.i += 1;
        } else if self.at(2) == '\'' && self.at(1) != '\n' {
            self.i += 3;
        } else {
            self.i += 1;
        }
    }

    /// C++ `R"delim( ... )delim"`. False when the text after `R"` is no delimiter followed by `(`.
    fn cpp_raw(&mut self) -> bool {
        let mut k = 1;
        let mut delim = String::new();
        while k < 18 && !matches!(self.at(k), '(' | '\n' | '\0' | ' ' | ')' | '\\' | '"') {
            delim.push(self.at(k));
            k += 1;
        }
        if self.at(k) != '(' {
            return false;
        }
        let end = format!("){delim}\"");
        let start = self.line;
        self.i += k + 1;
        loop {
            if self.done() {
                self.simple(Trouble::OpenString, '"', start);
                return true;
            }
            if self.starts(&end) {
                self.i += end.chars().count();
                return true;
            }
            if self.at(0) == '\n' {
                self.line += 1;
            }
            self.i += 1;
        }
    }

    /// C# `@"..."`: no escapes, `""` is a quote, a line break is part of it.
    fn verbatim(&mut self) {
        let start = self.line;
        self.i += 1;
        loop {
            if self.done() {
                self.simple(Trouble::OpenString, '"', start);
                return;
            }
            if self.at(0) == '"' {
                if self.at(1) == '"' {
                    self.i += 2;
                    continue;
                }
                self.i += 1;
                return;
            }
            if self.at(0) == '\n' {
                self.line += 1;
            }
            self.i += 1;
        }
    }

    /// The body of a JavaScript template string (the opening backtick is behind): up to the closing backtick, or up to a `${`, which goes on the stack as a bracket.
    fn template_body(&mut self) {
        let start = self.line;
        loop {
            if self.done() {
                self.simple(Trouble::OpenString, '`', start);
                return;
            }
            let ch = self.at(0);
            if ch == '\\' {
                if self.at(1) == '\n' {
                    self.line += 1;
                }
                self.i = (self.i + 2).min(self.c.len());
                continue;
            }
            if ch == '`' {
                self.i += 1;
                return;
            }
            if ch == '$' && self.at(1) == '{' {
                let line = self.line;
                self.stack.push(Frame { ch: '{', line, hole: true });
                self.i += 2;
                return;
            }
            if ch == '\n' {
                self.line += 1;
            }
            self.i += 1;
        }
    }

    /// JavaScript: a `/` that starts a regular expression (`/[/"]+/g`) is skipped as a string would be. False for a division, a JSX closing tag, and a `/` with no end on its line.
    fn regex_literal(&mut self) -> bool {
        if self.at(1) == '/' || self.at(1) == '*' || !self.regex_allowed() {
            return false;
        }
        let mut k = 1;
        let mut class = false;
        loop {
            match self.at(k) {
                '\n' | '\0' => return false,
                '\\' => k += 1,
                '[' => class = true,
                ']' => class = false,
                '/' if !class => break,
                _ => {}
            }
            k += 1;
        }
        k += 1;
        while self.at(k).is_ascii_alphabetic() {
            k += 1;
        }
        self.i += k;
        true
    }

    /// Can a regular expression start here? After an operator, an opening bracket, a comma, a semicolon, or one of the keywords that take a value; not after a name, a number or a closer.
    fn regex_allowed(&self) -> bool {
        let mut k = self.i;
        while k > 0 && self.c[k - 1].is_whitespace() {
            k -= 1;
        }
        if k == 0 {
            return true;
        }
        let p = self.c[k - 1];
        if "(,=:[!&|?{;+-*%~^".contains(p) {
            return true;
        }
        if p.is_alphabetic() {
            let mut s = k;
            while s > 0 && is_ident(self.c[s - 1]) {
                s -= 1;
            }
            let word: String = self.c[s..k].iter().collect();
            return matches!(word.as_str(), "return" | "typeof" | "case" | "in" | "of" | "delete" | "void" | "throw" | "new" | "else" | "do" | "yield" | "await");
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(gate: Gate, name: &str, old: &str, new: &str) -> Option<String> {
        match check_change(gate, name, old, new) {
            Verdict::Note(n) => Some(n),
            _ => None,
        }
    }

    fn refused(name: &str, old: &str, new: &str) -> Option<String> {
        match check_change(Gate::On, name, old, new) {
            Verdict::Refuse(r) => Some(r),
            _ => None,
        }
    }

    #[test]
    fn valid_json_made_invalid_is_refused() {
        let old = "{\n  \"name\": \"a\",\n  \"version\": \"1\"\n}\n";
        let new = "{\n  \"name\": \"a\",\n  \"version\": \"1\",\n}\n";
        let why = refused("package.json", old, new).expect("a trailing comma breaks the file");
        assert!(why.contains("syntax error introduced at line 4"), "{why}");
        assert!(why.contains("invalid JSON"), "{why}");
        // The same change in `warn` is a note and no refusal.
        let n = note(Gate::Warn, "package.json", old, new).expect("a note");
        assert!(n.starts_with("package.json: syntax error introduced at line 4") && n.contains("written as given"), "{n}");
        // Off: nothing is looked at.
        assert_eq!(check_change(Gate::Off, "package.json", old, new), Verdict::Fine);
    }

    #[test]
    fn already_invalid_json_is_not_blocked() {
        let broken = "{\"a\": 1,}";
        assert_eq!(check_change(Gate::On, "x.json", broken, "{\"a\": 1,,}"), Verdict::Fine);
        assert_eq!(check_change(Gate::On, "x.json", broken, "{\"a\": 2}"), Verdict::Fine);
        // A file that is empty or does not exist has no earlier state to break, whatever is written.
        assert_eq!(check_change(Gate::On, "x.json", "", "{\"a\": "), Verdict::Fine);
        assert_eq!(check_change(Gate::On, "x.json", "  \n", "not json"), Verdict::Fine);
        // A byte order mark is not a syntax error.
        assert_eq!(check_change(Gate::On, "x.json", "\u{feff}{\"a\": 1}", "\u{feff}{\"a\": 2}"), Verdict::Fine);
    }

    #[test]
    fn unknown_extensions_pass() {
        for name in ["notes.txt", "README.md", "Makefile", "data.yaml", "page.html", "run.sh", "x.bin", "noext"] {
            assert_eq!(check_change(Gate::On, name, "{ ( [", "}} )) ]] \"unclosed"), Verdict::Fine, "{name}");
        }
    }

    #[test]
    fn json_that_holds_comments_by_its_name_is_not_parsed_as_json() {
        let old = "{\n  \"a\": 1\n}\n";
        let new = "{\n  // the reason\n  \"a\": 1\n}\n";
        for name in ["tsconfig.json", "app/tsconfig.base.json", ".vscode/settings.json", "x.jsonc", ".eslintrc.json", "jsconfig.json", ".devcontainer/devcontainer.json"] {
            assert_eq!(check_change(Gate::On, name, old, new), Verdict::Fine, "{name}");
        }
        assert!(refused("package.json", old, new).is_some(), "a comment in a plain .json file is a syntax error");
        // They are scanned for their brackets, though.
        let n = note(Gate::Warn, "tsconfig.json", old, "{\n  \"a\": 1\n").expect("a missing brace in a tsconfig is still a note");
        assert!(n.contains("`{` opened at line 1 is never closed"), "{n}");
    }

    #[test]
    fn a_text_over_a_mebibyte_is_not_looked_at() {
        let big = format!("[{}0]", "0,".repeat(MAX_BYTES / 2));
        assert!(big.len() > MAX_BYTES);
        assert_eq!(check_change(Gate::On, "x.json", &big, "{"), Verdict::Fine);
        assert_eq!(check_change(Gate::On, "x.json", "[1]", &big.replace("[0", "[")), Verdict::Fine);
    }

    #[test]
    fn a_removed_brace_is_noted_in_rust_and_ts() {
        let rs_old = "fn main() {\n    if true {\n        println!(\"{}\", 1);\n    }\n}\n";
        let rs_new = "fn main() {\n    if true {\n        println!(\"{}\", 1);\n}\n";
        let n = note(Gate::On, "src/main.rs", rs_old, rs_new).expect("a brace is missing");
        assert!(n.contains("the `{` opened at line 1 is never closed"), "{n}");
        let ts_old = "export function f(a: number) {\n  return (a + 1);\n}\n";
        let ts_new = "export function f(a: number) {\n  return (a + 1;\n}\n";
        let n = note(Gate::Warn, "src/f.ts", ts_old, ts_new).expect("a parenthesis is missing");
        assert!(n.contains("does not match the `(` opened at line 2") || n.contains("`(` opened at line 2 is never closed"), "{n}");
        // A change that keeps them balanced says nothing.
        assert_eq!(check_change(Gate::On, "src/main.rs", rs_old, "fn main() {\n    println!(\"hi\");\n}\n"), Verdict::Fine);
    }

    #[test]
    fn a_stray_closer_a_string_and_a_comment_that_never_end_are_noted() {
        let n = note(Gate::On, "a.go", "func f() {\n}\n", "func f() {\n}\n}\n").unwrap();
        assert!(n.contains("the `}` at line 3 closes nothing"), "{n}");
        let n = note(Gate::On, "a.java", "class A {\n  String s = \"a\";\n}\n", "class A {\n  String s = \"a;\n}\n").unwrap();
        assert!(n.contains("the string that starts at line 2 is not closed"), "{n}");
        let n = note(Gate::On, "a.c", "int x; /* one */\n", "int x; /* one \n").unwrap();
        assert!(n.contains("the comment that starts at line 1 is never closed"), "{n}");
        let n = note(Gate::On, "a.py", "print('a')\n", "print('a\n").unwrap();
        assert!(n.contains("the string that starts at line 1 is not closed"), "{n}");
    }

    #[test]
    fn a_preexisting_problem_is_not_blamed_on_the_change() {
        let before = "fn a() {\n    let s = \"x\";\n";
        // One `{` is open before; the change touches something else.
        assert_eq!(check_change(Gate::On, "a.rs", before, "fn a() {\n    let s = \"yy\";\n"), Verdict::Fine);
        // Another one open is a new problem of the same kind.
        let n = note(Gate::On, "a.rs", before, "fn a() {\n    let s = \"x\";\n    if s {\n").unwrap();
        assert!(n.contains("never closed"), "{n}");
    }

    #[test]
    fn rust_lifetimes_chars_raw_strings_and_nested_comments_do_not_confuse_the_scan() {
        let src = r####"
/* outer /* inner { */ still a comment ( */
struct S<'a> { x: &'a str }
impl<'a> S<'a> {
    fn f(&self) -> char { let c = '{'; let d = '\''; let e = '\u{1F600}'; let q = '"'; c }
    fn g(&self) -> &'static str { r#"a "quoted" ) string"# }
    fn h(&self) -> &str { r"C:\" }
    fn i(&self) -> &[u8] { b"bytes ] here" }
    fn j<'b>(&'b self) -> &'b str { "with // not a comment and { brace" }
}
"####;
        assert!(scan(src, Lang::Rust).is_empty(), "{:?}", scan(src, Lang::Rust));
        let mut broken = src.to_string();
        broken.push_str("fn k() {\n");
        assert_eq!(scan(&broken, Lang::Rust).len(), 1);
    }

    #[test]
    fn typescript_templates_regular_expressions_and_jsx_apostrophes_do_not_confuse_the_scan() {
        let ts = "const a = `x ${ b + `inner ${ c }` } y`;\nconst r = /[\"'(]+/g.test(s);\nconst d = x / 2 / y;\nfunction f() { return /\\/(}/; }\nconst o = { \"k\": 'v' };\n";
        assert!(scan(ts, Lang::Ts).is_empty(), "{:?}", scan(ts, Lang::Ts));
        let tsx = "export const A = () => (\n  <p>Don't {count} stop</p>\n);\nconst B = <br/>;\nconst C = <div>it's</div>;\n";
        assert!(scan(tsx, Lang::Js).is_empty(), "{:?}", scan(tsx, Lang::Js));
        // A template that is not closed is a problem.
        let open = scan("const a = `abc ${b};\n", Lang::Ts);
        assert!(!open.is_empty());
    }

    #[test]
    fn python_toml_and_css_are_scanned_by_their_own_rules() {
        let py = "def f(x):\n    \"\"\"Doc with ( and ' inside.\"\"\"\n    s = 'it\\'s'\n    t = f\"{x}\"  # a comment with ( in it\n    return [x, (1, 2)]\n";
        assert!(scan(py, Lang::Python).is_empty(), "{:?}", scan(py, Lang::Python));
        let toml = "[package]\nname = \"a\"\nlist = [\n  \"x\",\n  'y',\n]\nnote = \"\"\"\nplain text without equals\n\"\"\"\n[[bin]]\npath = 'src/m.rs'\ndeps = { a = 1, b = \"2\" } # trailing\n";
        assert!(scan(toml, Lang::Toml).is_empty(), "{:?}", scan(toml, Lang::Toml));
        let n = note(Gate::On, "Cargo.toml", toml, &toml.replace("name = \"a\"", "name \"a\"")).expect("a line with no equals sign");
        assert!(n.contains("line 2 is no `key = value`"), "{n}");
        let n = note(Gate::On, "Cargo.toml", toml, &toml.replace("[package]", "[package")).expect("a header that is not closed");
        assert!(n.contains("[") && n.contains("never closed"), "{n}");
        let css = "a > b { content: \"}\"; /* { */ color: red }\n";
        assert!(scan(css, Lang::Css).is_empty(), "{:?}", scan(css, Lang::Css));
    }

    #[test]
    fn go_c_cpp_and_csharp_strings_are_read_as_each_language_reads_them() {
        let go = "package a\nfunc f() { s := `raw ( { \"`; r := '\\''; _ = s + string(r) }\n";
        assert!(scan(go, Lang::Go).is_empty(), "{:?}", scan(go, Lang::Go));
        let cpp = "int a = 1'000'000; const char* s = R\"x(a ) \" ( {)x\"; char c = '{';\nint main() { return 0; }\n";
        assert!(scan(cpp, Lang::Cpp).is_empty(), "{:?}", scan(cpp, Lang::Cpp));
        let cs = "class A { string a = @\"line\n with \"\"quotes\"\" and { \"; string b = $\"{a}\"; }\n";
        assert!(scan(cs, Lang::CSharp).is_empty(), "{:?}", scan(cs, Lang::CSharp));
        let java = "class A { String t = \"\"\"\n  text ( block\n  \"\"\"; }\n";
        assert!(scan(java, Lang::Java).is_empty(), "{:?}", scan(java, Lang::Java));
    }

    #[test]
    fn the_scan_never_panics_on_any_text() {
        let langs = [Lang::Rust, Lang::Js, Lang::Ts, Lang::Go, Lang::C, Lang::Cpp, Lang::Java, Lang::CSharp, Lang::Kotlin, Lang::Python, Lang::Toml, Lang::Css, Lang::Jsonc];
        let pieces = ["'", "\"", "`", "${", "}", "{", "/*", "*/", "//", "#", "\\", "\n", "r#\"", "\"\"\"", "'''", "R\"(", "@\"", "/", "[", "]", "(", ")", "é", "\u{feff}", "$", "<", ">"];
        let mut seed = 0x2545F491u32;
        for lang in langs {
            for _ in 0..300 {
                let mut text = String::new();
                for _ in 0..40 {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    text.push_str(pieces[(seed >> 16) as usize % pieces.len()]);
                }
                let _ = scan(&text, lang);
            }
        }
    }

    /// The scan on the repository's own files (it is the measure of how often it would cry wolf): every source file of the crates and the scripts is read, and the share that scans as balanced
    /// is printed. Ignored in a normal run because it reads the checkout; run it with `cargo test -p aqua-native-tools --lib the_scan_on_this_repository -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads the whole checkout; run it by hand to measure how often the scan would cry wolf"]
    fn the_scan_on_this_repository_finds_its_files_balanced() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(rd) = std::fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                let name = e.file_name().to_string_lossy().to_string();
                if p.is_dir() {
                    if !matches!(name.as_str(), "target" | "node_modules" | ".git" | "vendor" | "data" | "logs" | "dist" | ".claude") {
                        walk(&p, out);
                    }
                } else {
                    out.push(p);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files = Vec::new();
        walk(&root, &mut files);
        let (mut seen, mut bad) = (0usize, Vec::new());
        for f in files {
            let name = f.to_string_lossy().to_string();
            let (_, kind) = kind_of(&name);
            let Kind::Scan(lang) = kind else { continue };
            let Ok(text) = std::fs::read_to_string(&f) else { continue };
            if text.len() > MAX_BYTES {
                continue;
            }
            seen += 1;
            if let Some(p) = scan(&text, lang).first() {
                bad.push(format!("{name}: {}", p.describe()));
            }
        }
        println!("{} of {} files scan as balanced", seen - bad.len(), seen);
        for b in bad.iter().take(60) {
            println!("  {b}");
        }
        assert!(bad.len() * 100 <= seen, "more than 1% of the files of the repository scan as unbalanced: {} of {seen}", bad.len());
    }
}

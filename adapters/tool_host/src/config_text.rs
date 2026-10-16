//! Reading the person's config files (card OA3-03; task EX-02 of the OpenCode audit).
//!
//! About fifteen files under `data/` are edited by hand (`tools/fs.json`, `policy.json`, `hooks.json`, `mcp.json` ...). Each of them was read by its own copy of
//! the same twenty lines: strict JSON only (a comment or a trailing comma, which the documentation's own examples invite, turned a tool family off), and an
//! error that existed only as a `warn!` line. This module is the part of the one loader that needs no knowledge of Aqua's types, so that the MCP adapters can use it too:
//!
//! * [`strip_jsonc`] accepts `//` and `/* */` comments and trailing commas. Comments become spaces and a trailing comma becomes a space, so **the output has the same
//!   length and the same line structure as the input**: the line and column that `serde_json` reports for the stripped text are the ones in the person's file. A
//!   string that is not closed, or a block comment that is not, is an error that names the line and column where it starts. Stripping happens before the typed
//!   parse (and so before `deny_unknown_fields`), so a comment can hide no key.
//! * [`ConfigError`] says which file, what kind of problem (could not be read, not valid JSON, wrong shape, rejected by a rule), and where. It is written by hand (no
//!   new dependency). Its message never repeats what the file holds: a value the parser objected to is left out (see [`scrub`]).
//! * [`suggest`] and [`unknown_keys`] give "did you mean" for a misspelt top-level key.
//! * [`write_atomic`] is the one durable writer of `fsutil`, used to write the defaults.
//!
//! No `{file:}` substitution is offered, and none must be added: a path read inside a loader is a new door.

use std::fmt;
use std::path::{Path, PathBuf};

/// The most a config file may hold. A file this large is not a configuration; it is refused rather than parsed.
pub const MAX_CONFIG_BYTES: usize = 1 << 20;

/// What went wrong with a config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigErrorKind {
    /// The file is there and could not be read (a lock, a permission, too large, not text).
    Unreadable,
    /// Not valid JSON (after comments and trailing commas are allowed): a missing comma, a string that is not closed.
    Syntax,
    /// Valid JSON of the wrong shape: an unknown or missing key, a value of the wrong type.
    Shape,
    /// The right shape, and a rule of the file's kind refused it (a wildcard where one is not allowed, a secret in the file).
    Invalid,
}

impl ConfigErrorKind {
    /// The word used in the status route and in the doctor.
    pub fn word(self) -> &'static str {
        match self {
            ConfigErrorKind::Unreadable => "unreadable",
            ConfigErrorKind::Syntax => "syntax",
            ConfigErrorKind::Shape => "shape",
            ConfigErrorKind::Invalid => "invalid",
        }
    }
}

/// A config file that cannot be used, and why. `file` is empty until [`ConfigError::in_file`] names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub file: PathBuf,
    pub kind: ConfigErrorKind,
    /// 1-based, in the file as the person wrote it (comments included).
    pub line: Option<u32>,
    pub column: Option<u32>,
    /// What is wrong, without the line and column and without any value from the file.
    pub message: String,
}

impl ConfigError {
    pub fn new(kind: ConfigErrorKind, message: impl Into<String>) -> Self {
        Self { file: PathBuf::new(), kind, line: None, column: None, message: message.into() }
    }

    pub fn at(mut self, line: u32, column: u32) -> Self {
        self.line = Some(line);
        self.column = Some(column);
        self
    }

    pub fn in_file(mut self, file: &Path) -> Self {
        self.file = file.to_path_buf();
        self
    }

    /// An error from a text made elsewhere (a validator, a parser that returns a `String`): the position `serde_json` appends (`at line 3 column 7`) becomes the position of
    /// the error, and a value that the text quotes from the file is left out.
    pub fn from_message(kind: ConfigErrorKind, text: &str) -> Self {
        let (body, position) = split_position(text);
        let mut e = Self::new(kind, scrub(body));
        if let Some((l, c)) = position {
            e = e.at(l, c);
        }
        e
    }

    /// An error of the JSON parser: its category decides the kind and its position is kept.
    pub fn from_serde(e: &serde_json::Error) -> Self {
        use serde_json::error::Category;
        let kind = match e.classify() {
            Category::Io => ConfigErrorKind::Unreadable,
            Category::Syntax | Category::Eof => ConfigErrorKind::Syntax,
            Category::Data => ConfigErrorKind::Shape,
        };
        let mut out = Self::from_message(kind, &e.to_string());
        if e.line() > 0 {
            out.line = Some(e.line() as u32);
            out.column = Some(e.column() as u32);
        }
        out
    }

    /// `line 12, column 5: expected `,` or `}`` (or just the message when no position is known). This is what the log and the doctor print after the file name.
    pub fn detail(&self) -> String {
        match (self.line, self.column) {
            (Some(l), Some(c)) => format!("line {l}, column {c}: {}", self.message),
            (Some(l), None) => format!("line {l}: {}", self.message),
            _ => self.message.clone(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.file.as_os_str().is_empty() {
            write!(f, "{}", self.detail())
        } else {
            write!(f, "{}: {}", self.file.display(), self.detail())
        }
    }
}

impl std::error::Error for ConfigError {}

/// Files saved by Notepad or Windows PowerShell start with a UTF-8 byte-order mark, which a JSON reader rejects.
pub fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// The 1-based line and column (in bytes, as `serde_json` counts) of the byte at `offset`.
fn position_of(src: &[u8], offset: usize) -> (u32, u32) {
    let before = &src[..offset.min(src.len())];
    let line = before.iter().filter(|b| **b == b'\n').count() as u32 + 1;
    let since = before.iter().rposition(|b| *b == b'\n').map(|p| before.len() - p - 1).unwrap_or(before.len());
    (line, since as u32 + 1)
}

/// The text with its comments and trailing commas made into spaces, and the byte-order mark removed. Everything else is untouched, so a string is never changed (a `//` inside
/// `"https://x"` is text) and the stripped text has the length and the lines of the original (less the mark).
///
/// Errors: a string that is not closed on its line, and a `/*` with no `*/`; both name the line and column where they start.
pub fn strip_jsonc(text: &str) -> Result<String, ConfigError> {
    let src = strip_bom(text).as_bytes();
    let len = src.len();
    let mut out: Vec<u8> = Vec::with_capacity(len);
    let mut commas: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < len {
        let b = src[i];
        match b {
            b'"' => {
                let start = i;
                out.push(b);
                i += 1;
                loop {
                    let unclosed = || {
                        let (l, c) = position_of(src, start);
                        ConfigError::new(ConfigErrorKind::Syntax, "a string is not closed on its line").at(l, c)
                    };
                    if i >= len {
                        return Err(unclosed());
                    }
                    match src[i] {
                        b'\\' => {
                            out.push(b'\\');
                            i += 1;
                            if i >= len || src[i] == b'\n' || src[i] == b'\r' {
                                return Err(unclosed());
                            }
                            out.push(src[i]);
                            i += 1;
                        }
                        b'"' => {
                            out.push(b'"');
                            i += 1;
                            break;
                        }
                        b'\n' | b'\r' => return Err(unclosed()),
                        c => {
                            out.push(c);
                            i += 1;
                        }
                    }
                }
            }
            b'/' if src.get(i + 1) == Some(&b'/') => {
                while i < len && src[i] != b'\n' && src[i] != b'\r' {
                    out.push(b' ');
                    i += 1;
                }
            }
            b'/' if src.get(i + 1) == Some(&b'*') => {
                let start = i;
                out.extend_from_slice(b"  ");
                i += 2;
                loop {
                    if i >= len {
                        let (l, c) = position_of(src, start);
                        return Err(ConfigError::new(ConfigErrorKind::Syntax, "a /* comment is not closed").at(l, c));
                    }
                    if src[i] == b'*' && src.get(i + 1) == Some(&b'/') {
                        out.extend_from_slice(b"  ");
                        i += 2;
                        break;
                    }
                    // A line break stays one, so the lines after the comment are numbered as the person sees them.
                    out.push(if src[i] == b'\n' || src[i] == b'\r' { src[i] } else { b' ' });
                    i += 1;
                }
            }
            b',' => {
                commas.push(out.len());
                out.push(b);
                i += 1;
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    // A comma with only white space (comments are white space now) between it and a closing bracket is a trailing comma.
    for at in commas {
        let mut j = at + 1;
        while j < out.len() && matches!(out[j], b' ' | b'\t' | b'\n' | b'\r') {
            j += 1;
        }
        if j < out.len() && (out[j] == b'}' || out[j] == b']') {
            out[at] = b' ';
        }
    }
    // Only whole bytes were copied or replaced by ASCII, and a comment never starts or ends inside a multi-byte character, so this holds.
    String::from_utf8(out).map_err(|_| ConfigError::new(ConfigErrorKind::Syntax, "the text is not valid UTF-8"))
}

/// What a loader hands to its parser: the byte-order mark removed, comments and trailing commas made into spaces.
pub fn prepare(text: &str) -> Result<String, ConfigError> {
    strip_jsonc(text)
}

/// Splits the position `serde_json` appends (`... at line 3 column 7`) from a message.
fn split_position(text: &str) -> (&str, Option<(u32, u32)>) {
    let Some(at) = text.rfind(" at line ") else { return (text, None) };
    let tail = &text[at + " at line ".len()..];
    let Some((line, col)) = tail.split_once(" column ") else { return (text, None) };
    match (line.trim().parse::<u32>(), col.trim().parse::<u32>()) {
        (Ok(l), Ok(c)) => (&text[..at], Some((l, c))),
        _ => (text, None),
    }
}

/// A key name that is safe to repeat: short, and letters, digits and a few signs. Anything else is not echoed.
fn safe_key(name: &str) -> bool {
    !name.is_empty() && name.len() <= 48 && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ' '))
}

/// Removes from a parser's message what the file holds. `serde_json` writes `invalid type: string "ghp_x", expected a map` and `unknown variant `x``: the value is cut and the
/// kind of value stays. A field name is kept when it is an ordinary one (that is what "did you mean" needs).
pub fn scrub(message: &str) -> String {
    let mut msg = message.to_string();
    for marker in ["invalid type: ", "invalid value: "] {
        if let Some(p) = msg.find(marker) {
            let from = p + marker.len();
            if let Some(q) = msg[from..].find(", expected ").map(|q| q + from) {
                let shown = msg[from..q].split(['"', '`']).next().unwrap_or("").trim().to_string();
                msg = format!("{}{}{}", &msg[..from], shown, &msg[q..]);
            }
        }
    }
    for marker in ["unknown variant `", "unknown field `", "duplicate field `", "missing field `"] {
        if let Some(p) = msg.find(marker) {
            let name_at = p + marker.len();
            if let Some(end) = msg[name_at..].find('`').map(|e| e + name_at) {
                let name = &msg[name_at..end];
                if marker.starts_with("unknown variant") {
                    // The value of the file is cut: `unknown variant, expected one of ...`.
                    msg = format!("{}unknown variant{}", &msg[..p], &msg[end + 1..]);
                } else if !safe_key(name) {
                    msg = format!("{}(an odd name){}", &msg[..name_at - 1], &msg[end + 1..]);
                }
            }
        }
    }
    if msg.chars().count() > 400 {
        msg = msg.chars().take(400).collect::<String>() + "...";
    }
    msg
}

/// The top-level keys of a JSON object that are not in `known`, each with the closest known key when one is near. `text` is the prepared text. A text that is not an object, or
/// not JSON, has none (the parse says so). At most 20 are given.
pub fn unknown_keys(text: &str, known: &[&str]) -> Vec<String> {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(text) else { return Vec::new() };
    map.keys()
        .filter(|k| !known.contains(&k.as_str()))
        .take(20)
        .map(|k| {
            if !safe_key(k) {
                return "an unknown key with an unusual name".to_string();
            }
            match suggest(k, known) {
                Some(s) => format!("unknown key `{k}`: did you mean `{s}`?"),
                None => format!("unknown key `{k}`"),
            }
        })
        .collect()
}

/// The known key closest to `key` when it is within two edits (a typo), else none. Ties go to the first in `known`.
pub fn suggest<'a>(key: &str, known: &[&'a str]) -> Option<&'a str> {
    let key = key.to_ascii_lowercase();
    known.iter().map(|k| (edit_distance(&key, &k.to_ascii_lowercase()), *k)).filter(|(d, _)| *d <= 2).min_by_key(|(d, _)| *d).map(|(_, k)| k)
}

/// Levenshtein distance between two short ASCII-ish words (counted in characters).
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Replaces `path` with `bytes` so that a reader sees the old file or the whole new one: a temporary file next to it, flushed, then renamed over it (`fsutil`). The temporary
/// file is removed on an error and the old file is untouched.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    crate::fsutil::write_atomic_durable(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn ok(text: &str) -> Value {
        serde_json::from_str(&strip_jsonc(text).expect("strips")).expect("valid JSON after stripping")
    }

    #[test]
    fn line_comments_block_comments_and_trailing_commas_are_accepted() {
        assert_eq!(ok("{\n // the folders\n \"a\": 1, /* two */ \"b\": [1, 2,],\n}\n"), json!({"a": 1, "b": [1, 2]}));
        assert_eq!(ok("{\"a\": {\"b\": [ {\"c\": 1,}, ], },}"), json!({"a": {"b": [{"c": 1}]}}));
        // Comments between the comma and the bracket, in any form.
        assert_eq!(ok("[1, // last\n]"), json!([1]));
        assert_eq!(ok("[1, /* last */ ]"), json!([1]));
        assert_eq!(ok("[1,\n  // a\n  /* b */\n]"), json!([1]));
        // A comma inside a string, or between values, is not touched.
        assert_eq!(ok("{\"a\": \"x,}\", \"b\": 2}"), json!({"a": "x,}", "b": 2}));
    }

    #[test]
    fn slashes_inside_a_string_are_text() {
        assert_eq!(ok("{\"url\": \"https://x.example/a//b\", \"c\": \"/* not a comment */\"}"), json!({"url": "https://x.example/a//b", "c": "/* not a comment */"}));
        // An escaped quote does not end the string, so a // after it is still inside.
        assert_eq!(ok(r#"{"a": "q\" // still text", "b": 1}"#), json!({"a": "q\" // still text", "b": 1}));
        // A backslash at the end of a string (an escaped backslash) does end it.
        assert_eq!(ok("{\"a\": \"p\\\\\" // comment\n}"), json!({"a": "p\\"}));
    }

    #[test]
    fn the_byte_order_mark_and_crlf_line_ends_are_handled() {
        assert_eq!(ok("\u{feff}// first line\r\n{\"a\": 1, // x\r\n}\r\n"), json!({"a": 1}));
        assert_eq!(strip_jsonc("\u{feff}{}").unwrap(), "{}");
    }

    #[test]
    fn the_stripped_text_has_the_length_and_the_lines_of_the_original() {
        let src = "{\r\n  // comment with a multi-byte letter: é and 日本\n  \"a\": 1, /* two\n lines */ \"b\": [\n 1,\n ],\n}\n";
        let out = strip_jsonc(src).unwrap();
        assert_eq!(out.len(), src.len(), "same length in bytes");
        let breaks = |s: &str| s.bytes().enumerate().filter(|(_, b)| *b == b'\n' || *b == b'\r').map(|(i, _)| i).collect::<Vec<_>>();
        assert_eq!(breaks(&out), breaks(src), "every line break where it was");
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), json!({"a": 1, "b": [1]}));
    }

    #[test]
    fn a_syntax_error_after_comments_is_reported_where_the_person_sees_it() {
        // The error is the missing comma on line 5, column 3, with comments and a block comment over two lines above it.
        let src = "{\n  // one\n  /* two\n     three */ \"a\": 1\n  \"b\": 2\n}";
        let stripped = strip_jsonc(src).unwrap();
        let e = serde_json::from_str::<Value>(&stripped).unwrap_err();
        let err = ConfigError::from_serde(&e);
        assert_eq!(err.kind, ConfigErrorKind::Syntax);
        assert_eq!((err.line, err.column), (Some(5), Some(3)), "{err}");
        assert!(err.detail().starts_with("line 5, column 3: expected"), "{}", err.detail());
        assert!(!err.message.contains(" at line "), "the position is a field, not part of the message: {}", err.message);
    }

    #[test]
    fn a_string_or_a_block_comment_that_is_not_closed_names_where_it_starts() {
        let e = strip_jsonc("{\n  \"a\": \"open\n}").unwrap_err();
        assert_eq!((e.kind, e.line, e.column), (ConfigErrorKind::Syntax, Some(2), Some(8)), "{e}");
        let e = strip_jsonc("{\"a\": 1}\n/* never closed\n").unwrap_err();
        assert_eq!((e.line, e.column), (Some(2), Some(1)), "{e}");
        assert!(e.message.contains("comment"), "{e}");
        let e = strip_jsonc("{\"a\": \"x\\").unwrap_err();
        assert_eq!((e.line, e.column), (Some(1), Some(7)), "{e}");
        // None of the messages repeats the text of the file.
        assert!(!strip_jsonc("{\"secret_word\": \"hunter2").unwrap_err().to_string().contains("hunter2"));
    }

    #[test]
    fn a_comment_cannot_hide_a_key() {
        // The comment is gone before the typed parse sees the text, so a key behind it is read (and refused by deny_unknown_fields) like any other.
        #[derive(serde::Deserialize, Debug)]
        #[serde(deny_unknown_fields)]
        struct Cfg {
            #[allow(dead_code)]
            a: u32,
        }
        let src = "{\"a\": 1, /* \"b\": 2, */ \"c\": 3}";
        let e = serde_json::from_str::<Cfg>(&strip_jsonc(src).unwrap()).unwrap_err();
        assert!(e.to_string().contains("unknown field `c`"), "{e}");
        // And a key inside the comment is not one.
        assert!(serde_json::from_str::<Cfg>(&strip_jsonc("{\"a\": 1, /* \"b\": 2 */}").unwrap()).is_ok());
    }

    #[test]
    fn a_lone_slash_and_other_text_are_left_for_the_parser_to_refuse() {
        let out = strip_jsonc("{\"a\": 1 / 2}").unwrap();
        assert_eq!(out, "{\"a\": 1 / 2}");
        assert!(serde_json::from_str::<Value>(&out).is_err());
        assert_eq!(strip_jsonc("").unwrap(), "");
    }

    #[test]
    fn what_the_parser_quotes_from_the_file_is_cut_from_the_message() {
        #[derive(serde::Deserialize, Debug)]
        #[serde(deny_unknown_fields)]
        struct Cfg {
            #[allow(dead_code)]
            n: u32,
            #[allow(dead_code)]
            mode: Option<Mode>,
        }
        #[derive(serde::Deserialize, Debug)]
        #[serde(rename_all = "snake_case")]
        enum Mode {
            Fast,
        }
        let _ = Mode::Fast;
        for (src, must, never) in [
            (r#"{"n": "ghp_planted_token_value"}"#, "invalid type: string, expected u32", "ghp_planted"),
            (r#"{"n": 1, "mode": "planted_variant_value"}"#, "unknown variant, expected", "planted_variant"),
            (r#"{"n": 1, "ghp_planted_key_with_a_long_name_that_is_over_the_limit_of_forty_eight": 2}"#, "unknown field", "ghp_planted_key"),
            (r#"{"n": 1, "tokén": 2}"#, "(an odd name)", "tokén"),
            (r#"{"n": -5}"#, "invalid value: integer, expected u32", "-5"),
        ] {
            let e = ConfigError::from_serde(&serde_json::from_str::<Cfg>(src).unwrap_err());
            assert_eq!(e.kind, ConfigErrorKind::Shape, "{src}");
            assert!(e.message.contains(must), "{src}: {}", e.message);
            assert!(!e.message.contains(never) && !e.detail().contains(never), "{src}: {}", e.detail());
        }
        // An ordinary field name stays: it is what the person has to fix.
        let e = ConfigError::from_serde(&serde_json::from_str::<Cfg>(r#"{"n": 1, "mod": 2}"#).unwrap_err());
        assert!(e.message.contains("unknown field `mod`"), "{}", e.message);
    }

    #[test]
    fn a_message_made_elsewhere_gives_up_its_position() {
        let e = ConfigError::from_message(ConfigErrorKind::Invalid, "not valid: expected `,` or `}` at line 12 column 5");
        assert_eq!((e.line, e.column, e.message.as_str()), (Some(12), Some(5), "not valid: expected `,` or `}`"));
        assert_eq!(e.detail(), "line 12, column 5: not valid: expected `,` or `}`");
        let e = ConfigError::from_message(ConfigErrorKind::Invalid, "a rule refused it");
        assert_eq!((e.line, e.detail().as_str()), (None, "a rule refused it"));
        let named = e.in_file(Path::new("tools/fs.json"));
        assert_eq!(named.to_string(), "tools/fs.json: a rule refused it");
    }

    #[test]
    fn a_misspelt_key_gets_the_closest_known_one() {
        let known = ["roots", "sensitive_names", "limits"];
        assert_eq!(suggest("rots", &known), Some("roots"));
        assert_eq!(suggest("Limits", &known), Some("limits"));
        assert_eq!(suggest("sensitive_name", &known), Some("sensitive_names"));
        assert_eq!(suggest("completely_different", &known), None);
        let notes = unknown_keys(r#"{"rots": [], "limits": {}, "zzz": 1}"#, &known);
        assert_eq!(notes, vec!["unknown key `rots`: did you mean `roots`?".to_string(), "unknown key `zzz`".to_string()]);
        assert!(unknown_keys("[1]", &known).is_empty() && unknown_keys("{ nope", &known).is_empty());
        assert_eq!(unknown_keys("{\"a\u{e9}\": 1}", &known), vec!["an unknown key with an unusual name".to_string()]);
    }

    #[test]
    fn an_atomic_write_leaves_no_temporary_file_and_a_failed_one_leaves_the_old_file() {
        let dir = std::env::temp_dir().join(format!("aqua_config_text_atomic_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("tools").join("x.json");
        write_atomic(&path, b"{\"v\": 1}").unwrap();
        write_atomic(&path, b"{\"v\": 2}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"v\": 2}");
        let names: Vec<String> = std::fs::read_dir(path.parent().unwrap()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["x.json".to_string()], "no temporary file after a success");
        // A failure between writing the temporary file and renaming it: the old file stays and the temporary one is gone.
        let failed = crate::fsutil::write_atomic_durable_with(&path, b"{\"v\": 3}", |_| Err(std::io::Error::other("disk full")));
        assert!(failed.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"v\": 2}", "the old file is untouched");
        let names: Vec<String> = std::fs::read_dir(path.parent().unwrap()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["x.json".to_string()], "no temporary file after a failure");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! The front matter of a text file (`SKILL.md`, and the commands and agents that come later): a small, tolerant subset of YAML (card OA11-01; task EX-07 of the OpenCode audit).
//!
//! ```text
//! ---
//! name: release-notes
//! description: >
//!   Write the release notes for a version
//!   from the git log.
//! allowed-tools: [git.log, git.diff]
//! metadata:
//!   author: me
//!   version: "2"
//! ---
//! The text of the file.
//! ```
//!
//! What it reads, and nothing more (it is not a YAML parser, and it never runs anything, builds an object from a tag or follows an anchor):
//!
//! * `key: value` at the start of a line. A plain value may go on over indented lines (they are joined with one space). Quotes (`"a\n b"`, `'it''s'`) are taken off. `#` at the start of a
//!   line (after its indent) is a comment; a `#` inside a value is part of the value, as it was in the first reader of skills.
//! * `|` (lines kept) and `>` (lines folded into one) block scalars, with the chomping signs (`|-`, `>+`) accepted and the text trimmed at both ends.
//! * a list, as `[a, b, "c d"]` (it may go on over lines) or as `- item` lines, indented or not.
//! * a map one level deep (`metadata:` and indented `key: value` lines under it).
//! * a duplicate key is an error. Whether a key is one the reader knows is the reader's business: it decides between an error and a warning.
//!
//! The file may have a byte order mark and Windows line ends. The text after the closing line of three dashes is the body.

/// What a key holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Scalar(String),
    List(Vec<String>),
    /// A map one level deep, in the order written.
    Map(Vec<(String, String)>),
}

impl Value {
    /// The text of a scalar, `None` for a list or a map.
    pub fn as_scalar(&self) -> Option<&str> {
        match self {
            Value::Scalar(s) => Some(s),
            _ => None,
        }
    }

    /// A list as items. A scalar is one item per piece of a comma or space separated text (`allowed-tools: git.log, git.diff`); a map is none.
    pub fn items(&self) -> Vec<String> {
        match self {
            Value::List(l) => l.clone(),
            Value::Scalar(s) => s.split([',', ' ', '\n']).map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect(),
            Value::Map(_) => Vec::new(),
        }
    }

    /// The scalar as one line: every run of white space (a folded block, a value that went on over lines) is one space.
    pub fn one_line(&self) -> Option<String> {
        self.as_scalar().map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
    }
}

/// A file split into its front matter and its text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Document {
    /// The keys in the order written.
    pub fields: Vec<(String, Value)>,
    /// What follows the front matter, trimmed.
    pub body: String,
}

impl Document {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn scalar(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_scalar)
    }
}

/// The most keys one file may have.
const MAX_FIELDS: usize = 64;

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

/// A key: letters, digits, `-`, `_` and `.`.
fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 64 && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Takes the quotes off a value that is wholly quoted. A value that is not quoted is returned trimmed. `Err` for a quote that is not closed or text after the closing quote.
fn unquote(raw: &str) -> Result<String, String> {
    let v = raw.trim();
    let mut chars = v.chars();
    match chars.next() {
        Some('"') => {
            let mut out = String::new();
            let mut escaped = false;
            let mut closed_at = None;
            for (i, c) in v.char_indices().skip(1) {
                if escaped {
                    out.push(match c {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        '0' => '\0',
                        other => other,
                    });
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    closed_at = Some(i + 1);
                    break;
                } else {
                    out.push(c);
                }
            }
            let end = closed_at.ok_or_else(|| format!("the quoted text {v:?} is not closed"))?;
            let rest = v[end..].trim();
            if !rest.is_empty() && !rest.starts_with('#') {
                return Err(format!("there is text after the closing quote of {v:?}"));
            }
            Ok(out)
        }
        Some('\'') => {
            let mut out = String::new();
            let body: Vec<char> = v.chars().skip(1).collect();
            let mut i = 0;
            let mut closed_at = None;
            while i < body.len() {
                if body[i] == '\'' {
                    if body.get(i + 1) == Some(&'\'') {
                        out.push('\'');
                        i += 2;
                        continue;
                    }
                    closed_at = Some(i + 1);
                    break;
                }
                out.push(body[i]);
                i += 1;
            }
            let end = closed_at.ok_or_else(|| format!("the quoted text {v:?} is not closed"))?;
            let rest: String = body[end..].iter().collect();
            let rest = rest.trim();
            if !rest.is_empty() && !rest.starts_with('#') {
                return Err(format!("there is text after the closing quote of {v:?}"));
            }
            Ok(out)
        }
        _ => Ok(v.to_string()),
    }
}

/// Splits the inside of `[ ... ]` on the commas that are not in quotes.
fn split_flow(inner: &str) -> Result<Vec<String>, String> {
    let mut items = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in inner.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if escaped {
                    escaped = false;
                } else if c == '\\' && q == '"' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
            }
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            None if c == ',' => {
                items.push(std::mem::take(&mut cur));
            }
            None => cur.push(c),
        }
    }
    if quote.is_some() {
        return Err(format!("a quote in the list [{inner}] is not closed"));
    }
    items.push(cur);
    items.into_iter().map(|i| unquote(&i)).filter(|r| r.as_ref().map_or(true, |s| !s.is_empty())).collect()
}

/// The lines after `from` that belong to a value: indented lines, blank lines and comments between them. The index of the first line after them.
fn nested_end(lines: &[&str], from: usize) -> usize {
    let mut end = from;
    let mut i = from;
    while i < lines.len() {
        let l = lines[i];
        if is_blank(l) || is_comment(l) {
            i += 1;
            continue;
        }
        if indent_of(l) > 0 {
            i += 1;
            end = i;
        } else {
            break;
        }
    }
    end
}

/// A block scalar: the lines below a `|` or `>`.
fn block_scalar(lines: &[&str], folded: bool) -> String {
    let content: Vec<&str> = lines.iter().copied().filter(|l| !is_blank(l)).collect();
    let indent = content.iter().map(|l| indent_of(l)).min().unwrap_or(0);
    let cut = |l: &str| -> String {
        if is_blank(l) {
            String::new()
        } else {
            l.chars().skip(indent).collect::<String>().trim_end().to_string()
        }
    };
    let rows: Vec<String> = lines.iter().map(|l| cut(l)).collect();
    let text = if folded {
        let mut out = String::new();
        let mut gap = 0;
        for row in &rows {
            if row.is_empty() {
                gap += 1;
                continue;
            }
            if !out.is_empty() {
                out.push_str(&if gap > 0 { "\n".repeat(gap) } else { " ".to_string() });
            }
            gap = 0;
            out.push_str(row);
        }
        out
    } else {
        rows.join("\n")
    };
    text.trim().to_string()
}

/// A block indicator after `|` or `>`: only chomping signs and an indent digit.
fn is_block_indicator(rest: &str) -> Option<bool> {
    let mut chars = rest.chars();
    let folded = match chars.next()? {
        '|' => false,
        '>' => true,
        _ => return None,
    };
    chars.all(|c| matches!(c, '+' | '-') || c.is_ascii_digit()).then_some(folded)
}

/// Reads a file: `Err` says what is wrong in words for the person who wrote it.
pub fn parse(text: &str) -> Result<Document, String> {
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let mut all: Vec<&str> = text.split('\n').collect();
    // Blank lines before the opening line are let through.
    while all.first().is_some_and(|l| is_blank(l)) {
        all.remove(0);
    }
    if all.first().map(|l| l.trim_end()) != Some("---") {
        return Err("it must start with a line of three dashes (the front matter)".into());
    }
    let close = all.iter().skip(1).position(|l| l.trim_end() == "---").map(|p| p + 1).ok_or("the front matter has no closing line of three dashes")?;
    let lines = &all[1..close];
    let body = all[close + 1..].join("\n").trim().to_string();

    let mut fields: Vec<(String, Value)> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if is_blank(line) || is_comment(line) {
            i += 1;
            continue;
        }
        let t = line.trim();
        if indent_of(line) > 0 {
            return Err(format!("front matter line {t:?} is indented but follows no key"));
        }
        let Some((k, rest)) = line.split_once(':') else { return Err(format!("front matter line {t:?} is not `key: value`")) };
        let key = k.trim();
        if !valid_key(key) {
            return Err(format!("front matter line {t:?} is not `key: value`"));
        }
        if fields.iter().any(|(f, _)| f == key) {
            return Err(format!("the key {key:?} is given twice"));
        }
        if fields.len() >= MAX_FIELDS {
            return Err(format!("more than {MAX_FIELDS} keys"));
        }
        let rest = rest.trim();
        i += 1;
        let value = if rest.is_empty() {
            // The value is on the lines below: a list, a map, or a plain text that goes on.
            let mut end = nested_end(lines, i);
            // A list may be written without an indent under its key.
            if end == i {
                let mut j = i;
                while j < lines.len() {
                    let l = lines[j];
                    if l.starts_with("- ") || l.trim_end() == "-" {
                        j += 1;
                        end = j;
                    } else if is_blank(l) || is_comment(l) {
                        j += 1;
                    } else {
                        break;
                    }
                }
            }
            let nested: Vec<&str> = lines[i..end].iter().copied().filter(|l| !is_blank(l) && !is_comment(l)).collect();
            i = end;
            match nested.first().map(|l| l.trim_start()) {
                None => Value::Scalar(String::new()),
                Some(first) if first.starts_with("- ") || first == "-" => {
                    let mut items = Vec::new();
                    for l in &nested {
                        let l = l.trim_start();
                        let Some(item) = l.strip_prefix('-') else { return Err(format!("front matter line {:?} is in a list of {key:?} but does not start with `- `", l)) };
                        let item = unquote(item)?;
                        if !item.is_empty() {
                            items.push(item);
                        }
                    }
                    Value::List(items)
                }
                Some(first) if first.split_once(':').is_some_and(|(k, _)| valid_key(k.trim())) => {
                    let mut pairs: Vec<(String, String)> = Vec::new();
                    for l in &nested {
                        let Some((k, v)) = l.trim_start().split_once(':') else { return Err(format!("front matter line {:?} is in the map {key:?} but is not `key: value`", l.trim())) };
                        let k = k.trim().to_string();
                        if !valid_key(&k) {
                            return Err(format!("front matter line {:?} is in the map {key:?} but is not `key: value`", l.trim()));
                        }
                        if pairs.iter().any(|(p, _)| *p == k) {
                            return Err(format!("the key {k:?} is given twice in {key:?}"));
                        }
                        pairs.push((k, unquote(v)?));
                    }
                    Value::Map(pairs)
                }
                Some(_) => Value::Scalar(nested.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" ")),
            }
        } else if let Some(folded) = is_block_indicator(rest) {
            let end = nested_end(lines, i);
            let block = &lines[i..end];
            i = end;
            Value::Scalar(block_scalar(block, folded))
        } else if rest.starts_with('[') {
            // A flow list, which may go on over lines until its `]`.
            let mut joined = rest.to_string();
            while !joined.trim_end().ends_with(']') {
                if i >= lines.len() {
                    return Err(format!("the list of {key:?} that starts with [ is not closed with ]"));
                }
                joined.push(' ');
                joined.push_str(lines[i].trim());
                i += 1;
            }
            let inner = joined.trim_end().trim_start_matches('[').trim_end_matches(']');
            Value::List(split_flow(inner)?)
        } else if rest.starts_with('"') || rest.starts_with('\'') {
            // Quoted text that goes on over lines is joined with a space until the quote closes.
            let mut joined = rest.to_string();
            loop {
                match unquote(&joined) {
                    Ok(v) => break Value::Scalar(v),
                    Err(e) if e.contains("is not closed") && i < lines.len() => {
                        joined.push(' ');
                        joined.push_str(lines[i].trim());
                        i += 1;
                    }
                    Err(e) => return Err(format!("{key}: {e}")),
                }
            }
        } else {
            // Plain text: indented lines after it go on with it.
            let mut text = rest.to_string();
            while i < lines.len() && indent_of(lines[i]) > 0 && !is_blank(lines[i]) && !is_comment(lines[i]) {
                text.push(' ');
                text.push_str(lines[i].trim());
                i += 1;
            }
            Value::Scalar(text)
        };
        fields.push((key.to_string(), value));
    }
    Ok(Document { fields, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Value {
        Value::Scalar(v.into())
    }

    fn list(v: &[&str]) -> Value {
        Value::List(v.iter().map(|x| x.to_string()).collect())
    }

    #[test]
    fn plain_and_quoted_values_a_comment_and_the_text_after_the_front_matter() {
        let d = parse("---\n# a comment\nname: release-notes\ndescription: \"Say \\\"hi\\\"\\nthen go\"\nother: 'it''s'\nlicense: MIT # not a comment: it is part of the value\n\n---\nThe text.\n\nSecond paragraph.\n").unwrap();
        assert_eq!(d.scalar("name"), Some("release-notes"));
        assert_eq!(d.scalar("description"), Some("Say \"hi\"\nthen go"));
        assert_eq!(d.scalar("other"), Some("it's"));
        assert_eq!(d.scalar("license"), Some("MIT # not a comment: it is part of the value"));
        assert_eq!(d.body, "The text.\n\nSecond paragraph.");
        assert_eq!(d.fields.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), vec!["name", "description", "other", "license"], "the order written is kept");
    }

    #[test]
    fn a_plain_value_may_go_on_over_indented_lines_and_a_colon_inside_a_value_is_kept() {
        let d = parse("---\ndescription: Write the notes\n  from the log: all of it\n  and group them\nname: x\n---\nt").unwrap();
        assert_eq!(d.scalar("description"), Some("Write the notes from the log: all of it and group them"));
        assert_eq!(d.scalar("name"), Some("x"));
    }

    #[test]
    fn block_scalars_keep_or_fold_their_lines() {
        let d = parse("---\nfolded: >\n  one\n  two\n\n  three\nkept: |\n  a\n   b\n  c\nstrip: >-\n  x\n  y\nname: n\n---\nt").unwrap();
        assert_eq!(d.scalar("folded"), Some("one two\nthree"));
        assert_eq!(d.scalar("kept"), Some("a\n b\nc"));
        assert_eq!(d.scalar("strip"), Some("x y"));
        assert_eq!(d.get("folded").unwrap().one_line().as_deref(), Some("one two three"));
        assert_eq!(d.scalar("name"), Some("n"));
    }

    #[test]
    fn lists_are_written_in_brackets_or_as_dash_lines_indented_or_not() {
        let d = parse("---\na: [x, \"y, z\", 'w']\nb:\n  - one\n  - \"two\"\nc:\n- left\n- flush\nd: [p,\n  q]\ne: git.log, git.diff fs.read\nf: []\n---\nt").unwrap();
        assert_eq!(d.get("a"), Some(&list(&["x", "y, z", "w"])));
        assert_eq!(d.get("b"), Some(&list(&["one", "two"])));
        assert_eq!(d.get("c"), Some(&list(&["left", "flush"])), "a list may start at the left edge");
        assert_eq!(d.get("d"), Some(&list(&["p", "q"])), "a bracketed list may go on over lines");
        assert_eq!(d.get("e").unwrap().items(), vec!["git.log", "git.diff", "fs.read"], "a plain text is one item per word when it is read as a list");
        assert_eq!(d.get("f"), Some(&list(&[])));
    }

    #[test]
    fn a_map_is_one_level_deep() {
        let d = parse("---\nmetadata:\n  author: me\n  version: \"2\"\nname: n\n---\nt").unwrap();
        assert_eq!(d.get("metadata"), Some(&Value::Map(vec![("author".into(), "me".into()), ("version".into(), "2".into())])));
        assert_eq!(d.scalar("name"), Some("n"));
        assert!(parse("---\nmetadata:\n  a: 1\n  a: 2\n---\nt").unwrap_err().contains("twice in \"metadata\""));
    }

    #[test]
    fn windows_line_ends_and_a_byte_order_mark_and_blank_lines_before_the_front_matter() {
        let d = parse("\u{feff}\r\n---\r\nname: a\r\ndescription: d\r\nallowed-tools:\r\n  - fs.read\r\n---\r\nbody\r\nmore\r\n").unwrap();
        assert_eq!((d.scalar("name"), d.scalar("description")), (Some("a"), Some("d")));
        assert_eq!(d.get("allowed-tools"), Some(&list(&["fs.read"])));
        assert_eq!(d.body, "body\nmore");
    }

    #[test]
    fn a_file_that_is_not_in_the_format_says_what_is_wrong() {
        let bad = |t: &str| parse(t).unwrap_err();
        assert!(bad("no front matter\n").contains("three dashes"));
        assert!(bad("---\nname: x\n").contains("no closing line"));
        assert!(bad("---\nname: x\nname: y\n---\nt").contains("given twice"));
        assert!(bad("---\nnot a pair\n---\nt").contains("not `key: value`"));
        assert!(bad("---\nbad key: 1\n---\nt").contains("not `key: value`"));
        assert!(bad("---\n  indented: 1\n---\nt").contains("follows no key"));
        assert!(bad("---\na: \"open\n---\nt").contains("not closed"));
        assert!(bad("---\na: \"x\" y\n---\nt").contains("after the closing quote"));
        assert!(bad("---\na: [x, y\n---\nt").contains("not closed with ]"));
        let many = (0..65).map(|i| format!("k{i}: v")).collect::<Vec<_>>().join("\n");
        assert!(bad(&format!("---\n{many}\n---\nt")).contains("more than 64 keys"));
    }

    #[test]
    fn what_looks_like_markup_is_text_and_nothing_is_run_or_followed() {
        // A YAML tag, an anchor and an alias are only text here.
        let d = parse("---\nname: !!python/object/apply:os.system [rm]\nref: &a x\nuse: *a\n---\nt").unwrap();
        assert_eq!(d.scalar("name"), Some("!!python/object/apply:os.system [rm]"));
        assert_eq!((d.scalar("ref"), d.scalar("use")), (Some("&a x"), Some("*a")));
    }

    #[test]
    fn the_front_matter_of_every_skill_of_this_repository_parses() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(".claude").join("skills");
        let mut seen = 0;
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let file = entry.path().join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&file) else { continue };
            let d = parse(&text).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            assert!(d.scalar("name").is_some_and(|n| !n.is_empty()) && d.scalar("description").is_some_and(|n| !n.is_empty()), "{}", file.display());
            assert!(!d.body.is_empty(), "{}", file.display());
            seen += 1;
        }
        assert!(seen >= 5, "{seen} skills read from {}", dir.display());
        assert_eq!(s("x"), Value::Scalar("x".into()));
    }
}

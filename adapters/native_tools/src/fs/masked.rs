//! The view of a file of secrets that a read gives unless the person agrees to more (card OA8-03, SE-08).
//!
//! `fs.read` of a file whose name says it holds secrets (`.env`, `*.tfvars`, `.netrc`, `*.pem`, `id_rsa`...) used to put the whole file into the model's context once the person
//! said yes, including every value the redactor has no pattern for (`DATABASE_URL`, a vendor's key in a format nobody listed). The model almost never needs the values: it needs to
//! know which names are set, so it can tell why a program does not start. So the read shows `NAME=[REDACTED len=24]`: the names, the shape of the file, the comments, and for each
//! value only how long it is. `reveal: true` gives the real text; it asks the person every time (`FsProvider::always_ask_reason`).
//!
//! The view is built to fail closed. A line it does not understand is hidden whole (`[REDACTED line len=40]`), and so is every line of a key file or between `-----BEGIN` and
//! `-----END`. What it keeps: blank lines, lines of brackets only, section and block openers, the name of a `NAME=value` line, a name with no value (`FOO=` is how an unset
//! variable shows), and a comment. A comment that is itself a setting (`# OLD_KEY=abc`) is masked like one, and the patterns of the redactor run over the other comments;
//! a secret written in plain words in a comment is shown, and that is the one thing this view does not promise.

use super::guard::wildcard;
use regex::Regex;
use std::sync::OnceLock;

/// File names that are not `NAME=value` at all: every line but the `-----BEGIN`/`-----END` markers is hidden.
const WHOLE_LINE_FILES: &[&str] = &["*.pem", "*.key", "*.pfx", "*.p12", "*.kdbx", "*.ovpn", "id_rsa*", "id_ed25519*", "id_ecdsa*"];

/// Masks a file line by line, in order (it remembers whether it is between `-----BEGIN` and `-----END`). Make one per read and give it every line from the first, also the
/// ones the read skips, or a key that starts above the part that is shown would be taken for ordinary lines.
pub struct MaskedView {
    whole_lines: bool,
    in_block: bool,
}

fn setting_line() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r#"^(?P<head>[ \t]*(?:(?:export|set)[ \t]+)?(?P<key>"[^"]{1,64}"|'[^']{1,64}'|[A-Za-z_][A-Za-z0-9_.\-]{0,47})[ \t]*(?P<sep>[=:])[ \t]*)(?P<value>.*)$"#).expect("setting pattern compiles")
    })
}

/// The comment marker at the start of a line (`#`, `;`, or `// ` with a space: `//registry.example/:_authToken=` of an `.npmrc` is a setting), and the text after it.
fn comment_of(trimmed: &str) -> Option<(&str, &str)> {
    for marker in ["#", ";", "// "] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return Some((marker, rest));
        }
    }
    None
}

/// `-----BEGIN X-----` (Some(true)) or `-----END X-----` (Some(false)), and nothing else on the line.
fn block_marker(trimmed: &str) -> Option<bool> {
    let inner = trimmed.strip_prefix("-----")?.strip_suffix("-----")?;
    let (begin, label) = match (inner.strip_prefix("BEGIN "), inner.strip_prefix("END ")) {
        (Some(l), _) => (true, l),
        (_, Some(l)) => (false, l),
        _ => return None,
    };
    (!label.is_empty() && label.chars().all(|c| c.is_ascii_uppercase() || c == ' ')).then_some(begin)
}

/// A line with nothing to hide: only brackets, commas, dashes and the like (`{`, `},`, `],`, `---`).
fn is_structure(trimmed: &str) -> bool {
    trimmed.chars().all(|c| matches!(c, '{' | '}' | '[' | ']' | '(' | ')' | ',' | ';' | '-' | '.'))
}

/// A block or section opener: `database {`, `resource "aws_x" "y" {`, `[database]`. Only a name's own characters may stand in front of the bracket, so that a secret
/// that happens to end in one (`p@ss{`) or a JSON list of values (`["a","b"]`) is not taken for one.
fn is_opener(trimmed: &str) -> bool {
    let plain = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '"' | '\'' | ' '));
    if let Some(head) = trimmed.strip_suffix('{').or_else(|| trimmed.strip_suffix('[')) {
        return plain(head.trim_end());
    }
    trimmed.strip_prefix('[').and_then(|r| r.strip_suffix(']')).is_some_and(|inner| inner.len() <= 64 && plain(inner) && !inner.contains(['"', '\'']))
}

/// A name that looks like the name of an environment variable: capitals, digits, underscores.
fn is_env_name(key: &str) -> bool {
    key.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') && key.chars().any(|c| c.is_ascii_uppercase())
}

fn hidden_line(indent: &str, trimmed: &str) -> String {
    format!("{indent}[REDACTED line len={}]", trimmed.chars().count())
}

/// What a value is worth showing of itself: how long it is, without the quotes around it.
fn value_len(value: &str) -> usize {
    let v = value.trim();
    let v = ["\"", "'"].iter().find_map(|q| v.strip_prefix(q).and_then(|r| r.strip_suffix(q))).unwrap_or(v);
    v.chars().count()
}

impl MaskedView {
    /// The view for the file called `file_name`.
    pub fn for_file(file_name: &str) -> Self {
        Self { whole_lines: WHOLE_LINE_FILES.iter().any(|p| wildcard(p, file_name)), in_block: false }
    }

    /// The line as the read shows it. `raw` may end with a line break; the result does not.
    pub fn line(&mut self, raw: &str) -> String {
        let line = raw.trim_end_matches(['\r', '\n']);
        let trimmed = line.trim();
        let indent = &line[..line.len() - line.trim_start().len()];
        if let Some(begin) = block_marker(trimmed) {
            self.in_block = begin;
            return line.to_string();
        }
        if trimmed.is_empty() || is_structure(trimmed) {
            return line.to_string();
        }
        if self.in_block || self.whole_lines {
            return hidden_line(indent, trimmed);
        }
        if let Some((marker, text)) = comment_of(trimmed) {
            return self.comment(line, indent, marker, text);
        }
        if is_opener(trimmed) {
            return line.to_string();
        }
        match masked_setting(line) {
            Some(masked) => masked,
            None => hidden_line(indent, trimmed),
        }
    }

    /// A comment is shown. One that is itself a setting in the style of an environment file (`# OLD_KEY=abc`, `; DB_PASSWORD: abc`) is masked like one; any other has the patterns
    /// of the redactor run over it (a key of a known format is hidden wherever it is written).
    fn comment(&self, line: &str, indent: &str, marker: &str, text: &str) -> String {
        let text_trimmed = text.trim_start();
        let gap = &text[..text.len() - text_trimmed.len()];
        if let Some(caps) = setting_line().captures(text_trimmed) {
            let key = caps.name("key").map_or("", |m| m.as_str());
            let sep = caps.name("sep").map_or("", |m| m.as_str());
            if sep == "=" || is_env_name(key.trim_matches(['"', '\''])) {
                if let Some(masked) = masked_setting(text_trimmed) {
                    return format!("{indent}{marker}{gap}{masked}");
                }
            }
        }
        aqua_tool_host::secret_patterns::mask(line)
    }
}

/// `NAME=value`, `NAME: value`, `export NAME=value`, `"name": "value",`: the part up to and including the separator, then the value hidden. `None` when the line is not a
/// setting, or is one that should be hidden whole.
fn masked_setting(line: &str) -> Option<String> {
    let caps = setting_line().captures(line)?;
    let head = caps.name("head")?.as_str();
    let key = caps.name("key")?.as_str();
    let sep = caps.name("sep")?.as_str();
    let value = caps.name("value")?.as_str();
    let quoted_key = key.starts_with(['"', '\'']);
    let value_trimmed = value.trim();
    if value_trimmed.is_empty() {
        // `NAME:` opens a section (YAML) and `NAME=` is a variable that is not set; but `abc=` at the end of a line of base64 is neither, and a name in lower case with `=` is not one to trust.
        return (sep == ":" || quoted_key || is_env_name(key)).then(|| line.trim_end().to_string());
    }
    // The value of a JSON member that opens an object or a list, or a value that is only brackets and a comma (not dashes or dots: a password can be those).
    if value_trimmed.chars().all(|c| matches!(c, '{' | '}' | '[' | ']' | ',')) {
        return Some(line.trim_end().to_string());
    }
    // A key of base64 is not a name: a long unbroken run of letters and digits before an `=` is the end of a line of a key, not a setting.
    if sep == "=" && !quoted_key && key.len() > 40 {
        return None;
    }
    // The comma after a JSON member's value is the file's own, not the secret's.
    let (value_text, comma) = match value_trimmed.strip_suffix(',') {
        Some(v) if quoted_key => (v, ","),
        _ => (value_trimmed, ""),
    };
    Some(format!("{head}[REDACTED len={}]{comma}", value_len(value_text)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(file: &str, text: &str) -> Vec<String> {
        let mut v = MaskedView::for_file(file);
        text.lines().map(|l| v.line(l)).collect()
    }

    #[test]
    fn an_env_file_keeps_its_names_and_the_length_of_each_value() {
        let out = view(
            ".env",
            "# database\nDATABASE_URL=db.internal/main\nexport STRIPE_KEY=\"abc def\"\nEMPTY=\nPORT = 8080\n\n; old\n# OLD_KEY=abcdefgh\nlower_case=abc\n",
        );
        assert_eq!(
            out,
            vec![
                "# database",
                "DATABASE_URL=[REDACTED len=16]",
                "export STRIPE_KEY=[REDACTED len=7]",
                "EMPTY=",
                "PORT = [REDACTED len=4]",
                "",
                "; old",
                "# OLD_KEY=[REDACTED len=8]",
                "lower_case=[REDACTED len=3]",
            ]
        );
    }

    #[test]
    fn structured_files_keep_their_shape_and_hide_their_values() {
        let json = view("credentials.json", "{\n  \"client_id\": \"abc123\",\n  \"nested\": {\n    \"secret\": \"s3cr3t\"\n  },\n  \"list\": [\n    \"x\"\n  ]\n}\n");
        assert_eq!(json[0], "{");
        assert_eq!(json[1], "  \"client_id\": [REDACTED len=6],");
        assert_eq!(json[2], "  \"nested\": {");
        assert_eq!(json[3], "    \"secret\": [REDACTED len=6]");
        assert_eq!(json[4], "  },");
        assert_eq!(json[5], "  \"list\": [");
        assert_eq!(json[6], "    [REDACTED line len=3]", "a bare list item is hidden whole");
        let yaml = view("secrets.yaml", "database:\n  password: hunter2hunter2\n  host: db.internal\n");
        assert_eq!(yaml, vec!["database:", "  password: [REDACTED len=14]", "  host: [REDACTED len=11]"]);
        let tf = view("prod.tfvars", "region = \"eu-west-1\"\ndb {\n  pass = \"x1\"\n}\n");
        assert_eq!(tf, vec!["region = [REDACTED len=9]", "db {", "  pass = [REDACTED len=2]", "}"]);
    }

    #[test]
    fn a_line_it_does_not_understand_is_hidden_whole() {
        let net = view(".netrc", "machine example.com login bob password hunter2\n");
        assert_eq!(net, vec!["[REDACTED line len=46]"]);
        let npm = view(".npmrc", "//registry.example.org/:_authToken=0f9c-legacy-uuid-token\nregistry=https://registry.example.org/\n");
        assert_eq!(npm[0], "[REDACTED line len=57]");
        assert_eq!(npm[1], "registry=[REDACTED len=29]");
        let pg = view(".pgpass", "db.example.com:5432:app:bob:hunter2\n");
        assert_eq!(pg, vec!["db.example.com:[REDACTED len=20]"]);
    }

    #[test]
    fn a_key_file_shows_its_markers_and_nothing_between_them() {
        // The markers are written in pieces, so that the scan before a commit does not take this file for a key.
        let (begin, end) = (["-----BEGIN RSA ", "PRIVATE KEY-----"].concat(), ["-----END RSA ", "PRIVATE KEY-----"].concat());
        let pem = view("deploy.pem", &format!("{begin}\nMIIEowIBAAKCAQEAxyzxyzxyzxyzxyzxyzxyzxyz\nabcDEF==\n{end}\n"));
        assert_eq!(pem, vec![begin.clone(), "[REDACTED line len=40]".to_string(), "[REDACTED line len=8]".to_string(), end.clone()]);
        // The same block inside a file that is read as settings is hidden the same way, also when the block's first line is not in the part the read shows.
        let mixed = view(".env", &format!("A=1\n{begin}\nabcDEF==\nMIIEowIBAAKCAQEA\n{end}\nB=2\n"));
        assert_eq!(mixed[1], begin);
        assert_eq!(mixed[2], "[REDACTED line len=8]");
        assert_eq!(mixed[3], "[REDACTED line len=16]");
        assert_eq!(mixed[5], "B=[REDACTED len=1]");
        // A line of base64 that ends in `=` is not a setting even when no marker was seen.
        let tail = view(".env", "abcDEFghiJKLmnoPQRstuVWXyz0123456789abcdefghijk=\n");
        assert_eq!(tail, vec!["[REDACTED line len=48]"]);
    }

    #[test]
    fn a_comment_is_shown_but_a_known_key_inside_it_is_not() {
        let key = ["gh", "p_", "abcdefghijklmnopqrstuvwxyzABCDEF0123456"].concat();
        let out = view(".env", &format!("# rotate this: {key}\n# Note: set before running\n"));
        assert!(out[0].starts_with("# rotate this: ") && !out[0].contains(&key), "{}", out[0]);
        assert_eq!(out[1], "# Note: set before running", "prose with a colon is not a setting");
    }
}

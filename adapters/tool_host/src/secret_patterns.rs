//! The one list of secret formats (card OA8-03, SE-07).
//!
//! Two programs look for secrets: the redactor, which masks them in everything a tool returns before the model, the screen or the log sees it
//! (`output::Redactor`), and the scan before a commit (`scripts/hooks/pre-commit.ps1`). They used to keep two lists, and the commit gate knew
//! formats the redactor did not (Google keys, npm tokens, `Basic` headers, a login inside a URL). Now there is one: this table.
//!
//! * The redactor uses every row: `pattern` is a regular expression of the `regex` crate, and `shape` says what is masked.
//! * The commit scan reads the rows that say `scan` from `scripts/hooks/secret_patterns.txt`, which a test generates from this table and keeps current
//!   (`AQUA_UPDATE_DOCS=1 cargo test -p aqua-tool-host --lib secret_patterns` rewrites it). A row may give the scan a different pattern (`Scan::With`), because a
//!   gate that blocks a commit wants other thresholds than a mask on output: the scan is stricter in length (it flags `glpat-` and 8 characters; the mask waits for 20)
//!   and must not flag prose (`Bearer` needs 24 characters there).
//! * A pattern keeps to the part of the syntax that the `regex` crate and .NET share: no look-around, no back-references, plain `(?i)` and `(?m)` flags.
//!
//! The rows are checked in order, so a specific format is masked whole before the general `name=value` rule sees what is left of it. Every replacement is
//! `[REDACTED]`, a mask that is there already (`[REDACTED]`, or `[REDACTED len=12]` from the masked view of a file of secrets) is left as it is, and nothing else the
//! patterns match is in a mask, so masking text twice gives what masking once gives (a test tries it on thousands of mixtures).

use crate::output::REDACTED;
use regex::{Captures, Regex};
use std::borrow::Cow;
use std::sync::OnceLock;

/// What is masked of a match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// All of it: the match becomes `[REDACTED]`.
    Whole,
    /// Group 1 stays, then `[REDACTED]`, then group 2 when the pattern has one: `password=` and the `@` after a URL's login stay, the value between goes.
    Keep,
}

/// Whether the scan before a commit looks for this format, and with which pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scan {
    /// Not in the scan: the format is too common in honest text to block a commit on (`password: x`, a header line).
    Off,
    /// The scan uses `pattern` as it is.
    Same,
    /// The scan uses this pattern instead (a .NET regular expression; it is matched case-sensitively, so a pattern that wants either case starts with `(?i)`).
    With(&'static str),
}

#[derive(Debug, Clone, Copy)]
pub struct SecretPattern {
    /// What the commit scan says it found (never the text itself).
    pub kind: &'static str,
    pub pattern: &'static str,
    pub shape: Shape,
    pub scan: Scan,
}

const fn row(kind: &'static str, pattern: &'static str, shape: Shape, scan: Scan) -> SecretPattern {
    SecretPattern { kind, pattern, shape, scan }
}

/// The list. The order is the order the redactor applies the rows.
pub const PATTERNS: &[SecretPattern] = &[
    // A quoted JSON member whose name says it is a credential: `"access_token": "abc..."`. The plain `name=value` rule cannot match it, because the closing quote of the name
    // sits between the name and the colon, and an API response is exactly this shape. The whole value goes, up to its closing quote (an escaped quote does not end it).
    row(
        "secret-named JSON member",
        r#"(?i)("[^"\s]*(?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|authorization|private[_-]?key|passphrase|cookie)[^"\s]*"\s*:\s*")(?:(?:[^"\\]|\\.)+)(")"#,
        Shape::Keep,
        Scan::Off,
    ),
    row(
        "private key block",
        r"-----BEGIN [A-Z ]*PRIVATE KEY(?: BLOCK)?-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY(?: BLOCK)?-----",
        Shape::Whole,
        Scan::With(r"(?i)-----BEGIN [A-Z ]*PRIVATE KEY(?: BLOCK)?-----"),
    ),
    // A key whose end was cut off (a read that stopped at its line limit, a pipe that closed): the lines of its body are masked all the same.
    row("private key block, cut short", r"-----BEGIN [A-Z ]*PRIVATE KEY(?: BLOCK)?-----(?:\r?\n[A-Za-z0-9+/=]{16,}){2,}", Shape::Whole, Scan::Off),
    row(
        "JSON web token",
        r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
        Shape::Whole,
        Scan::With(r"\beyJ[A-Za-z0-9_\-]{10,}\.eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{8,}"),
    ),
    row("API key (sk-)", r"\bsk-[A-Za-z0-9_-]{20,}", Shape::Whole, Scan::With(r"(?i)\bsk-[A-Za-z0-9_\-]{16,}")),
    row("Stripe key", r"\b(?:[sr]k_(?:live|test)_|whsec_)[A-Za-z0-9]{16,}", Shape::Whole, Scan::Same),
    row("GitHub token", r"\bgh[pousr]_[A-Za-z0-9]{30,}", Shape::Whole, Scan::With(r"(?i)\bgh[po]_[A-Za-z0-9]{8,}")),
    row("GitHub fine-grained token", r"\bgithub_pat_[A-Za-z0-9_]{40,}", Shape::Whole, Scan::With(r"(?i)\bgithub_pat_[A-Za-z0-9_]{20,}")),
    row("GitLab token", r"\bglpat-[A-Za-z0-9_-]{20,}", Shape::Whole, Scan::With(r"(?i)glpat-[A-Za-z0-9_\-]{8,}")),
    row("AWS access key id", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", Shape::Whole, Scan::With(r"(?i)AKIA[0-9A-Z]{12,}")),
    row("Slack token", r"\bxox[abprs]-[A-Za-z0-9-]{10,}", Shape::Whole, Scan::With(r"(?i)\bxox[abprs]-[A-Za-z0-9-]{10,}")),
    row("Hugging Face token", r"\bhf_[A-Za-z0-9]{30,}", Shape::Whole, Scan::Same),
    row("Google API key", r"\bAIza[0-9A-Za-z_-]{35}", Shape::Whole, Scan::With(r"(?i)\bAIza[0-9A-Za-z_\-]{35}")),
    row("Google OAuth token", r"\bya29\.[0-9A-Za-z_-]{20,}", Shape::Whole, Scan::Same),
    row("npm token", r"\bnpm_[A-Za-z0-9]{36}\b", Shape::Whole, Scan::With(r"(?i)\bnpm_[A-Za-z0-9]{36}\b")),
    row("PyPI token", r"\bpypi-[A-Za-z0-9_-]{50,}", Shape::Whole, Scan::Same),
    row("SendGrid key", r"\bSG\.[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}", Shape::Whole, Scan::Same),
    row("Bearer credential", r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{16,}", Shape::Whole, Scan::With(r"(?i)\bBearer\s+[A-Za-z0-9._~+/\-]{24,}")),
    // The scan wants a digit, a plus or a slash in the token (a long word after "Basic" is prose); the mask has no look-around to ask for it, and a word of 16 letters after "Basic" is rare enough to mask.
    row(
        "Basic credential",
        r"\bBasic\s+[A-Za-z0-9+/]{16,}={0,2}",
        Shape::Whole,
        Scan::With(r"(?i)\bBasic\s+(?=[A-Za-z0-9+/]*[0-9+/])[A-Za-z0-9+/]{16,}={0,2}"),
    ),
    // A header line, whole: `Authorization: Basic ...`, `Cookie: a=1; b=2`, `Set-Cookie: sid=...; Path=/`. At the start of a line (with the `> ` of a verbose curl), or inside a quote (`-H "Cookie: ..."`).
    row("HTTP credential header", r"(?im)(^[ \t>]*(?:proxy-authorization|authorization|cookie|set-cookie)[ \t]*:[ \t]*)[^\r\n]+", Shape::Keep, Scan::Off),
    row("HTTP credential header, quoted", r#"(?i)(["'][ \t]*(?:proxy-authorization|authorization|cookie|set-cookie)[ \t]*:[ \t]*)[^\r\n"']+"#, Shape::Keep, Scan::Off),
    // The password of a login written in a URL: `https://user:password@host`, a connection string (`postgres://app:pw@db/main`).
    row("login inside a URL", r"(://[^/@\s:]+:)[^/@\s]+(@)", Shape::Keep, Scan::With(r"://[^/@\s:]+:[^/@\s]+@")),
    row(
        "token in a URL query",
        r"(?i)([?&](?:access_token|auth_token|id_token|refresh_token|token|api_key|apikey|password|passwd|secret|client_secret|sig|signature|x-amz-signature|x-amz-security-token)=)[A-Za-z0-9._~%+/=-]{4,}",
        Shape::Keep,
        Scan::With(r"(?i)[?&](?:access_token|auth_token|token|api_key|apikey|password|secret)=[A-Za-z0-9._~%\-]{16,}"),
    ),
    // An Azure storage or service-bus key in a connection string.
    row("Azure key", r"(?i)(\b(?:AccountKey|SharedAccessKey)\s*=\s*)[A-Za-z0-9+/=]{16,}", Shape::Keep, Scan::Off),
    // The general rule, last: a name that says it holds a credential, then `=` or `:`, then the value. A quoted value goes whole (it may hold spaces); an unquoted one goes up to a space, comma, semicolon or `&`.
    row(
        "secret-named value",
        r#"(?i)((?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key)\s*[=:]\s*)(?:\[REDACTED len=[0-9]{1,9}\][^\s,;&]*|"(?:[^"\\\r\n]|\\.)*"|'(?:[^'\\\r\n]|\\.)*'|[^\s,;&]{4,})"#,
        Shape::Keep,
        Scan::Off,
    ),
];

/// The patterns compiled, each with its shape.
pub fn compiled() -> &'static [(Regex, Shape)] {
    static C: OnceLock<Vec<(Regex, Shape)>> = OnceLock::new();
    C.get_or_init(|| PATTERNS.iter().map(|p| (Regex::new(p.pattern).unwrap_or_else(|e| panic!("secret pattern for {} does not compile: {e}", p.kind)), p.shape)).collect())
}

/// Is `value` a mask that was put there already: `[REDACTED]`, or the form the view of a file of secrets uses (`[REDACTED len=12]`)? Only the exact forms count, so that text
/// which merely begins like one (`[REDACTEDhunter2`) is still masked.
fn is_placeholder(value: &str) -> bool {
    value == REDACTED || value.strip_prefix("[REDACTED len=").and_then(|r| r.strip_suffix(']')).is_some_and(|d| !d.is_empty() && d.len() <= 9 && d.bytes().all(|b| b.is_ascii_digit()))
}

/// `text` with everything the list knows masked.
pub fn mask(text: &str) -> String {
    let mut out = text.to_string();
    for (re, shape) in compiled() {
        let shape = *shape;
        let replaced = re.replace_all(&out, |caps: &Captures| match shape {
            Shape::Whole => REDACTED.to_string(),
            Shape::Keep => {
                let whole = caps.get(0).map_or("", |m| m.as_str());
                let (g1, g2) = (caps.get(1).map_or("", |m| m.as_str()), caps.get(2).map_or("", |m| m.as_str()));
                // What follows the kept start, up to the kept end, is the secret; if that is a mask already (the masked view of a file puts `[REDACTED len=12]` after a name), it stays as it is.
                match whole.strip_prefix(g1).and_then(|r| r.strip_suffix(g2)) {
                    Some(value) if is_placeholder(value) => whole.to_string(),
                    _ => format!("{g1}{REDACTED}{g2}"),
                }
            }
        });
        if let Cow::Owned(s) = replaced {
            out = s;
        }
    }
    out
}

/// The rules of the commit scan: what it says it found, and the pattern.
pub fn scan_rules() -> Vec<(&'static str, &'static str)> {
    PATTERNS
        .iter()
        .filter_map(|p| match p.scan {
            Scan::Off => None,
            Scan::Same => Some((p.kind, p.pattern)),
            Scan::With(own) => Some((p.kind, own)),
        })
        .collect()
}

/// The text of `scripts/hooks/secret_patterns.txt`: a comment, then one `kind<TAB>pattern` line per rule of the scan.
pub fn scan_file_text() -> String {
    let mut out = String::new();
    out.push_str("# The rules of the secret scan before a commit (scripts/hooks/pre-commit.ps1), one per line: kind, a tab, a .NET regular expression.\n");
    out.push_str("# Generated from adapters/tool_host/src/secret_patterns.rs, the list the redactor uses too. Do not edit by hand; rewrite it with\n");
    out.push_str("#   AQUA_UPDATE_DOCS=1 cargo test -p aqua-tool-host --lib secret_patterns\n");
    out.push_str("# A pattern is matched case-sensitively; one that wants either case starts with (?i). The scan never prints what a pattern found.\n");
    for (kind, pattern) in scan_rules() {
        out.push_str(kind);
        out.push('\t');
        out.push_str(pattern);
        out.push('\n');
    }
    out
}

/// A line that looks like it holds a secret: where, and what it looked like. Never the text itself (card OA7-02, TM-07).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// 1-based, in the text that was scanned.
    pub line: usize,
    /// One of the `kind` names of the table.
    pub kind: &'static str,
}

/// What a match of a scan rule must also satisfy: the one thing the .NET pattern of the commit hook asks with a look-ahead, which the `regex` crate has none of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Also {
    Nothing,
    /// The token after `Basic` has a digit, a plus or a slash in it (a long word after "Basic" is prose, not a login).
    TokenHasDigitPlusOrSlash,
}

/// The look-ahead that the hook's `Basic` rule carries.
const BASIC_LOOKAHEAD: &str = "(?=[A-Za-z0-9+/]*[0-9+/])";

/// The rules of the commit scan, compiled for the `regex` crate: the same rules `scan_rules` hands to the hook, with the one look-ahead turned into a check on the match.
fn scan_compiled() -> &'static [(&'static str, Regex, Also)] {
    static C: OnceLock<Vec<(&'static str, Regex, Also)>> = OnceLock::new();
    C.get_or_init(|| {
        scan_rules()
            .into_iter()
            .map(|(kind, pattern)| {
                let (pattern, also) = if pattern.contains(BASIC_LOOKAHEAD) { (pattern.replace(BASIC_LOOKAHEAD, ""), Also::TokenHasDigitPlusOrSlash) } else { (pattern.to_string(), Also::Nothing) };
                let re = Regex::new(&pattern).unwrap_or_else(|e| panic!("scan pattern for {kind} does not compile for the regex crate: {e}"));
                (kind, re, also)
            })
            .collect()
    })
}

/// The kinds of secret that one line looks like, by the rules of the commit scan (never the text). The marker `secret-scan: ignore` that the commit hook honours for a person's made-up
/// values is NOT honoured here: it is a text the model can write itself, and a gate it could switch off by typing is not one.
pub fn scan_line(line: &str) -> Vec<&'static str> {
    let mut kinds: Vec<&'static str> = Vec::new();
    for (kind, re, also) in scan_compiled() {
        let hit = re.find_iter(line).any(|m| match also {
            Also::Nothing => true,
            Also::TokenHasDigitPlusOrSlash => m.as_str().split_once(char::is_whitespace).is_some_and(|(_, token)| token.trim_start().chars().any(|c| c.is_ascii_digit() || c == '+' || c == '/')),
        });
        if hit && !kinds.contains(kind) {
            kinds.push(kind);
        }
    }
    kinds
}

/// Every line of `text` that looks like it holds a secret, by the rules of the commit scan (see `scan_line`).
pub fn scan(text: &str) -> Vec<Finding> {
    text.lines().enumerate().flat_map(|(i, line)| scan_line(line).into_iter().map(move |kind| Finding { line: i + 1, kind })).collect()
}

/// Shannon entropy of `s` in bits per character.
pub fn entropy_bits_per_char(s: &str) -> f64 {
    let mut counts = std::collections::HashMap::new();
    let mut n = 0usize;
    for c in s.chars() {
        *counts.entry(c).or_insert(0usize) += 1;
        n += 1;
    }
    if n == 0 {
        return 0.0;
    }
    counts.values().map(|&k| {
        let p = k as f64 / n as f64;
        -p * p.log2()
    }).sum()
}

/// The values shorter than this are not looked at by the entropy rule, and the ones with less entropy than `MIN_ENTROPY` are left as they are.
pub const ENTROPY_MIN_LEN: usize = 24;
pub const ENTROPY_MIN_BITS: f64 = 3.5;

fn assignment_line() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"(?m)^([ \t]*(?:export[ \t]+)?[A-Za-z_][A-Za-z0-9_.-]*[ \t]*=[ \t]*["']?)([^\s"']{24,})(["']?[ \t]*)$"#).expect("assignment pattern compiles"))
}

/// The optional rule for a text that is known to be a file of secrets: in a `NAME=value` line, a value of 24 characters or more with 3.5 bits of entropy per character or more
/// (a key, a token, a password a machine made) is masked even when no pattern above knows its format. Only for such files: on other text it would mask hashes, paths and long
/// words. Nothing calls it for text in general; the caller that knows what the text is says so.
pub fn mask_high_entropy_assignments(text: &str) -> String {
    assignment_line()
        .replace_all(text, |caps: &Captures| {
            let value = &caps[2];
            if entropy_bits_per_char(value) >= ENTROPY_MIN_BITS {
                format!("{}{}{}", &caps[1], REDACTED, &caps[3])
            } else {
                caps[0].to_string()
            }
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is the only copy: it must compile, and a row's shape must say what its groups are.
    #[test]
    fn every_row_compiles_and_has_the_groups_its_shape_needs() {
        let mut kinds = std::collections::HashSet::new();
        for p in PATTERNS {
            assert!(kinds.insert(p.kind), "two rows are called {}", p.kind);
            let re = Regex::new(p.pattern).unwrap_or_else(|e| panic!("{}: {e}", p.kind));
            let groups = re.captures_len() - 1;
            match p.shape {
                Shape::Whole => assert_eq!(groups, 0, "{} masks the whole match, so it keeps no group", p.kind),
                Shape::Keep => assert!((1..=2).contains(&groups), "{} keeps group 1 and maybe 2, it has {groups}", p.kind),
            }
        }
        // A scan pattern that does not use look-around also compiles here (the one with a look-ahead is for .NET only).
        for (kind, pattern) in scan_rules() {
            if !pattern.contains("(?=") {
                Regex::new(pattern).unwrap_or_else(|e| panic!("scan pattern of {kind}: {e}"));
            }
            assert!(pattern.is_ascii() && !pattern.contains(['\t', '\n', '\r']), "the scan file holds one ASCII line per rule: {kind}");
        }
    }

    /// Every kind the commit scan knew before the list was shared is still in it (a kind dropped from the table would silently stop blocking commits).
    #[test]
    fn the_scan_still_has_every_kind_it_had() {
        let scan: Vec<&str> = scan_rules().iter().map(|(k, _)| *k).collect();
        for kind in [
            "GitLab token", "GitHub token", "API key (sk-)", "AWS access key id", "private key block", "login inside a URL", "GitHub fine-grained token", "Slack token", "Google API key",
            "npm token", "JSON web token", "Bearer credential", "Basic credential", "token in a URL query",
        ] {
            assert!(scan.contains(&kind), "{kind} is no longer in the scan");
        }
    }

    /// The file the hook reads is the one this table makes. `AQUA_UPDATE_DOCS=1` rewrites it (as it does for the generated documents).
    #[test]
    fn the_file_the_commit_scan_reads_is_made_from_this_table() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/hooks/secret_patterns.txt");
        let want = scan_file_text();
        if std::env::var("AQUA_UPDATE_DOCS").is_ok_and(|v| v == "1") {
            std::fs::write(&path, &want).expect("write scripts/hooks/secret_patterns.txt");
            return;
        }
        let have = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("scripts/hooks/secret_patterns.txt cannot be read ({e}); AQUA_UPDATE_DOCS=1 cargo test -p aqua-tool-host --lib secret_patterns writes it")).replace("\r\n", "\n");
        assert_eq!(have, want, "scripts/hooks/secret_patterns.txt is not what the table makes; run AQUA_UPDATE_DOCS=1 cargo test -p aqua-tool-host --lib secret_patterns");
    }

    /// The masked view of a file of secrets writes `NAME=[REDACTED len=12]`; the general rule must not turn the name's value into a plain `[REDACTED]` and cut the rest off.
    #[test]
    fn a_mask_that_is_there_already_is_kept_and_text_that_only_begins_like_one_is_not() {
        for kept in ["DB_PASSWORD=[REDACTED len=12]", "api_key: [REDACTED len=40]", "Authorization: [REDACTED len=7]", "db_secret = [REDACTED]", "https://bob:[REDACTED]@example.test/x"] {
            assert_eq!(mask(kept), kept);
        }
        for leaked in ["DB_PASSWORD=[REDACTEDhunter2-secret", "Authorization: [REDACTED len=hunter2]", "token: [REDACTED len=12]hunter2"] {
            let out = mask(leaked);
            assert!(!out.contains("hunter2"), "{leaked} -> {out}");
        }
        assert_eq!(mask("api_key: [REDACTED len=12] hunter2"), "api_key: [REDACTED len=12] hunter2", "the word after the mask is not the value of the name");
    }

    /// Card OA7-02 (TM-07): the scan the git tools run before a commit is the rules of the commit hook, read by the `regex` crate. Fake values are put together here so that no line of
    /// this file is itself a secret to the hook.
    #[test]
    fn the_scan_finds_what_the_commit_hook_finds_and_says_where_without_the_text() {
        assert_eq!(scan_compiled().len(), scan_rules().len(), "every rule of the scan compiles for the regex crate");
        let gh = format!("{}{}", "ghp_", "a1".repeat(18));
        let bearer = format!("{} {}", "Authorization: Bearer", "Ab3".repeat(10));
        let basic = format!("{} {}", "Authorization: Basic", "dXNlcjpwYXNzd29yZDEyMzQ1Njc4");
        let key = ["-----BEGIN ", "RSA PRIVATE KEY-----"].concat();
        let url = format!("{}{}{}", "https://bot:made-up-", "password", "@example.invalid/x.git");
        let text = format!("fine line\ntoken = {gh}\n\n{bearer}\n{basic}\n{key}\nurl = {url}\nlet token = read_from_credential_store();\n");
        let found = scan(&text);
        let kinds: Vec<(usize, &str)> = found.iter().map(|f| (f.line, f.kind)).collect();
        assert!(kinds.contains(&(2, "GitHub token")), "{kinds:?}");
        assert!(kinds.contains(&(4, "Bearer credential")), "{kinds:?}");
        assert!(kinds.contains(&(5, "Basic credential")), "{kinds:?}");
        assert!(kinds.contains(&(6, "private key block")), "{kinds:?}");
        assert!(kinds.contains(&(7, "login inside a URL")), "{kinds:?}");
        assert!(found.iter().all(|f| f.line != 1 && f.line != 3 && f.line != 8), "prose and code that only mentions a token are not findings: {kinds:?}");
        // The hook lets a person mark a made-up value; the model could write that marker too, so the gate does not read it.
        assert!(!scan_line(&format!("{gh} secret-scan: ignore")).is_empty());
    }

    /// The `Basic` rule of the hook asks (with a look-ahead the `regex` crate has not) for a digit, a plus or a slash in the token: a long word after the word "Basic" is prose.
    #[test]
    fn a_long_word_after_basic_is_prose_and_a_login_is_not() {
        assert!(scan_line("Basic ConfigurationManagerSettings are in the manual").is_empty());
        assert!(scan_line("the Basic   AuthenticationHandlerFactory class").is_empty());
        assert_eq!(scan_line("Authorization: Basic dXNlcjpwYXNzd29yZDEyMzQ1Njc4"), vec!["Basic credential"]);
        assert_eq!(scan_line("basic AbcdefghijklmnopqrstuvwxYZ+/=="), vec!["Basic credential"]);
        assert!(scan_line("Basic short1").is_empty(), "under 16 characters");
    }

    #[test]
    fn the_entropy_rule_masks_a_random_looking_value_of_a_sensitive_file_and_nothing_else() {
        let key = ["Zk9fQ", "3xRmP7", "vT2nL8", "bW4yH6", "cJ1sD5aE"].concat();
        assert!(key.len() >= ENTROPY_MIN_LEN && entropy_bits_per_char(&key) >= ENTROPY_MIN_BITS, "{}", entropy_bits_per_char(&key));
        let text = format!("PORT=8080\nSTRIPE_KEY={key}\nexport OTHER=\"{key}\"\nNAME=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nPATHISH=/usr/local/lib/python3/site-packages\n");
        let out = mask_high_entropy_assignments(&text);
        assert!(!out.contains(&key), "{out}");
        assert!(out.contains("PORT=8080") && out.contains("STRIPE_KEY=[REDACTED]") && out.contains("export OTHER=\"[REDACTED]\""), "{out}");
        assert!(out.contains("NAME=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "a long value with little variety is not a key: {out}");
        assert_eq!(mask_high_entropy_assignments(&out), out, "twice is once");
    }
}

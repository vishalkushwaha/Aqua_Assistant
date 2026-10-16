//! Checks for the values that go on a git command line.
//!
//! Anything the model supplies is either validated here so git cannot read it as an option, or
//! passed after `--`. Revisions and branch names never start with `-`; paths are made relative to
//! the repository, checked against the filesystem guard, and passed after `--` with
//! `--literal-pathspecs` set so pathspec magic such as `:(top)` is not interpreted.

use aqua_tool_host::{ErrorCode, ToolError};

fn bad(msg: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArguments, msg)
}

/// A branch or tag name: letters, digits, `.`, `_`, `-`, `/`, no `..`, no leading `-` or `/`, no `.lock` suffix.
pub fn branch_name(s: &str) -> Result<&str, ToolError> {
    if s.is_empty() || s.len() > 200 {
        return Err(bad("a branch or tag name must be 1-200 characters"));
    }
    if s.starts_with(['-', '/', '.']) || s.ends_with(['/', '.']) || s.ends_with(".lock") {
        return Err(bad(format!("'{s}' is not a valid branch or tag name")));
    }
    if s.contains("..") || s.contains("//") || s.contains("@{") || s == "@" {
        return Err(bad(format!("'{s}' is not a valid branch or tag name")));
    }
    if !s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/')) {
        return Err(bad(format!("'{s}' contains characters that are not allowed in a branch or tag name")));
    }
    Ok(s)
}

/// A revision as read tools accept it: a name, a hash, `HEAD~3`, `main^`, `origin/main`, `abc..def`.
pub fn revision(s: &str) -> Result<&str, ToolError> {
    if s.is_empty() || s.len() > 200 {
        return Err(bad("a revision must be 1-200 characters"));
    }
    if s.starts_with('-') || s.starts_with('/') {
        return Err(bad(format!("'{s}' is not a valid revision (it may not start with '-')")));
    }
    if s.contains("...") || s.matches("..").count() > 1 {
        return Err(bad(format!("'{s}': use at most one '..' range")));
    }
    if !s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | '~' | '^' | '@' | '{' | '}')) {
        return Err(bad(format!("'{s}' contains characters that are not allowed in a revision")));
    }
    // `@{...}` may only be a plain reflog index such as `@{1}` or `HEAD@{2}`.
    if let Some(i) = s.find("@{") {
        let inner = &s[i + 2..];
        if !inner.strip_suffix('}').map(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit())).unwrap_or(false) {
            return Err(bad(format!("'{s}': only numeric reflog references like HEAD@{{2}} are allowed")));
        }
    }
    Ok(s)
}

/// A remote name as git stores it.
pub fn remote_name(s: &str) -> Result<&str, ToolError> {
    if s.is_empty() || s.len() > 100 || s.starts_with('-') || !s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
        return Err(bad(format!("'{s}' is not a valid remote name")));
    }
    Ok(s)
}

/// Text passed as a value (a commit message, a search term). It may contain anything except NUL,
/// because it always follows an option that takes a value.
pub fn text(s: &str, max: usize, what: &str) -> Result<(), ToolError> {
    if s.contains('\0') {
        return Err(bad(format!("{what} may not contain NUL characters")));
    }
    if s.chars().count() > max {
        return Err(bad(format!("{what} is longer than {max} characters")));
    }
    Ok(())
}

/// Replace `https://user:token@host/` style credentials in text git printed.
pub fn scrub_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        // Credentials sit between `://` and the first `@` that comes before the next `/` or whitespace.
        let end = tail.find(|c: char| c == '/' || c.is_whitespace()).unwrap_or(tail.len());
        match tail[..end].rfind('@') {
            Some(at) => {
                out.push_str("***@");
                rest = &tail[at + 1..];
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_cannot_be_read_as_options_or_tricks() {
        for ok in ["main", "feature/login-form", "release/1.2.3", "v1.0", "aqua/fix_42", "a"] {
            assert!(branch_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-f", "--upload-pack=calc", "-D", "/abs", ".hidden", "a..b", "a//b", "a b", "a;b", "a$b", "x@{1}", "@", "a.lock", "a/", "a.", "a\nb", "a~1", "a^", "a:b", &"x".repeat(201)] {
            assert!(branch_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn revisions_allow_the_usual_syntax_and_nothing_that_starts_an_option() {
        for ok in ["HEAD", "HEAD~3", "main^", "origin/main", "abc1234", "v1.0..v1.1", "main..feature", "HEAD@{2}", "@{1}", "HEAD^^", "stash@{0}"] {
            assert!(revision(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-p", "--output=x", "a...b", "a..b..c", "HEAD@{upstream}", "HEAD@{yesterday}", "@{push}", "a b", "a;b", "a`b", "a|b", "$(x)", "a:b", "a*"] {
            assert!(revision(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn remotes_and_text_are_checked() {
        assert!(remote_name("origin").is_ok() && remote_name("up-stream.2").is_ok());
        for bad in ["", "-o", "a b", "a/b", "http://x", "a;b"] {
            assert!(remote_name(bad).is_err(), "{bad}");
        }
        assert!(text("fix: a --flag-looking message", 100, "message").is_ok());
        assert!(text("a\0b", 100, "message").is_err() && text(&"x".repeat(101), 100, "message").is_err());
    }

    #[test]
    fn credentials_in_urls_are_masked() {
        assert_eq!(scrub_urls("origin\thttps://user:ghp_abc123@github.com/o/r.git (fetch)"), "origin\thttps://***@github.com/o/r.git (fetch)");
        assert_eq!(scrub_urls("https://tok@host/x and https://a:b@h2/y"), "https://***@host/x and https://***@h2/y");
        assert_eq!(scrub_urls("git@github.com:o/r.git and https://github.com/o/r.git"), "git@github.com:o/r.git and https://github.com/o/r.git");
        assert_eq!(scrub_urls("no urls here"), "no urls here");
        assert_eq!(scrub_urls("path/with@sign https://h/a@b"), "path/with@sign https://h/a@b");
    }
}

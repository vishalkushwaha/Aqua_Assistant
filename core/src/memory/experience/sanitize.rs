//! Keeps secrets and personal data out of stored experiences.
//!
//! A memory should describe the *approach*, not what was in the original task. Everything that goes
//! into the experience store passes through `redact` first, and text that is mostly secret is
//! refused outright by `is_safe_to_store`.

const REDACTED: &str = "[redacted]";

/// Words that mark the value after them (`password=...`, `token: ...`) as a secret.
const SECRET_KEYS: &[&str] = &[
    "password", "passwd", "pwd", "secret", "token", "api_key", "apikey", "api-key", "access_key",
    "private_key", "authorization", "bearer", "credential", "credentials", "cookie", "session",
    "client_secret", "auth",
];

/// Prefixes of well-known credential formats.
const TOKEN_PREFIXES: &[&str] = &[
    "sk-", "sk_", "pk_", "ghp_", "gho_", "ghu_", "ghs_", "github_pat_", "glpat-", "xoxb-", "xoxp-",
    "xoxa-", "akia", "asia", "aiza", "ya29.", "eyj", "npm_", "pypi-", "rk_", "whsec_",
];

/// Redact secrets and personal data from `text`. Returns the cleaned text and how many pieces were
/// removed. The surrounding words are kept so the approach still reads naturally.
pub fn redact(text: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut count = 0usize;
    let mut redact_next = false;
    let mut chars = text.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        if c.is_whitespace() {
            out.push(c);
            continue;
        }
        // A token runs to the next whitespace.
        let mut end = start + c.len_utf8();
        while let Some(&(i, n)) = chars.peek() {
            if n.is_whitespace() {
                break;
            }
            end = i + n.len_utf8();
            chars.next();
        }
        let token = &text[start..end];
        if redact_next && matches!(token.to_ascii_lowercase().trim_matches(|c: char| !c.is_ascii_alphabetic()), "bearer" | "basic" | "token") {
            // "Authorization: Bearer <token>": the scheme word stays, the token after it goes.
            out.push_str(token);
            continue;
        }
        if redact_next && !token.is_empty() {
            // "password: hunter2" -> the value after a secret key
            out.push_str(REDACTED);
            count += 1;
            redact_next = false;
            continue;
        }
        let (clean, n, next_is_secret) = redact_token(token);
        out.push_str(&clean);
        count += n;
        redact_next = next_is_secret;
    }
    (out, count)
}

/// Clean one whitespace-free token. Returns (text, redactions, whether the next token is a value).
fn redact_token(token: &str) -> (String, usize, bool) {
    let lower = token.to_ascii_lowercase();
    let trimmed = lower.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-' && c != '.');

    // key=value or key:value with a secret key. (URLs are handled below, parameter by parameter.)
    for sep in ['=', ':'] {
        if token.contains("://") {
            break;
        }
        if let Some(pos) = token.find(sep) {
            let (k, v) = token.split_at(pos);
            let v = &v[1..];
            let key = k.to_ascii_lowercase();
            let key = key.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
            if SECRET_KEYS.iter().any(|s| key == *s || key.ends_with(s)) && !key.is_empty() {
                if v.trim_matches(|c: char| !c.is_ascii_alphanumeric()).is_empty() {
                    // "password:" with the value in the next token
                    return (token.to_string(), 0, true);
                }
                return (format!("{k}{sep}{REDACTED}"), 1, false);
            }
        }
    }

    // URL with credentials in it: scheme://user:pass@host
    if let Some(scheme_end) = token.find("://") {
        let rest = &token[scheme_end + 3..];
        let authority = rest.split('/').next().unwrap_or("");
        if let Some(at) = authority.rfind('@') {
            let cleaned = format!("{}://{}{}", &token[..scheme_end], REDACTED_HOST, &rest[at + 1..]);
            return (cleaned, 1, false);
        }
        // Query strings often carry tokens: ?token=..., &key=...
        if let Some(q) = token.find('?') {
            let (base, query) = token.split_at(q);
            let mut changed = 0;
            let parts: Vec<String> = query[1..]
                .split('&')
                .map(|p| match p.split_once('=') {
                    Some((k, _)) if SECRET_KEYS.iter().any(|s| k.to_ascii_lowercase().contains(s)) => {
                        changed += 1;
                        format!("{k}={REDACTED}")
                    }
                    _ => p.to_string(),
                })
                .collect();
            if changed > 0 {
                return (format!("{base}?{}", parts.join("&")), changed, false);
            }
        }
    }

    // "Bearer <token>" and "Authorization: Basic <token>": the token is the following word.
    if trimmed == "bearer" || trimmed == "basic" {
        return (token.to_string(), 0, true);
    }

    // Known credential prefixes.
    // "akia"/"asia"/"aiza" are also ordinary word starts (Asia/Kolkata), so those need a key's shape.
    let key_shaped = |t: &str| t.len() >= 16 && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') && t.chars().any(|c| c.is_ascii_digit());
    if TOKEN_PREFIXES.iter().any(|p| trimmed.starts_with(p) && (!matches!(*p, "akia" | "asia" | "aiza") || key_shaped(&trimmed))) && trimmed.len() >= 12 {
        return (REDACTED.to_string(), 1, false);
    }

    // Email addresses.
    if let Some(at) = token.find('@') {
        let (local, domain) = token.split_at(at);
        if !local.is_empty() && domain.len() > 3 && domain[1..].contains('.') && !token.contains("://") {
            return ("[email]".to_string(), 1, false);
        }
    }

    // Long opaque strings (hex, base64, uuids concatenated) are almost always keys or hashes.
    let core = token.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    if core.len() >= 32 && core.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '+' || c == '/' || c == '=') {
        let digits = core.chars().filter(|c| c.is_ascii_digit()).count();
        let upper = core.chars().filter(|c| c.is_ascii_uppercase()).count();
        if digits >= 4 && (upper >= 2 || core.chars().all(|c| c.is_ascii_hexdigit())) {
            return (REDACTED.to_string(), 1, false);
        }
    }

    // Phone numbers: 10+ digits with only separators between them.
    let digit_count = core.chars().filter(|c| c.is_ascii_digit()).count();
    if digit_count >= 10 && core.chars().all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '(' | ')' | '.')) {
        return ("[phone]".to_string(), 1, false);
    }

    (token.to_string(), 0, false)
}

const REDACTED_HOST: &str = "[redacted]@";

/// Whether the text is fit to store after redaction: refuse when redaction removed so much that
/// little is left, since what remains would not be a useful approach anyway.
pub fn is_safe_to_store(original: &str, cleaned: &str, redactions: usize) -> bool {
    if cleaned.trim().is_empty() {
        return false;
    }
    if redactions == 0 {
        return true;
    }
    let words = cleaned.split_whitespace().count().max(1);
    // More than a third of the words were secrets: this is a credential dump, not an approach.
    redactions * 3 <= words && original.len() < 20_000
}

/// Redact and check in one step. `None` means the text must not be stored at all.
pub fn clean_for_storage(text: &str) -> Option<String> {
    let (cleaned, n) = redact(text);
    is_safe_to_store(text, &cleaned, n).then_some(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> String {
        redact(s).0
    }

    #[test]
    fn credential_shapes_are_removed_and_the_approach_is_kept() {
        assert_eq!(r("connect with password=hunter2 then run the query"), "connect with password=[redacted] then run the query");
        assert_eq!(r("send Authorization: Bearer abc123def456 to the API"), "send Authorization: Bearer [redacted] to the API");
        assert!(!r("use key sk-abcdefghijklmnop1234 here").contains("sk-abc"));
        assert!(!r("token ghp_abcdefghijklmnopqrstuvwxyz0123456789 leaked").contains("ghp_"));
        assert!(!r("clone https://user:secretpw@gitlab.com/a/b.git").contains("secretpw"));
        assert!(!r("open https://x.test/cb?token=abc123&page=2").contains("abc123"));
        assert!(r("open https://x.test/cb?token=abc123&page=2").contains("page=2"));
        assert!(!r("mail me at jane.doe@example.com please").contains("jane.doe"));
        assert!(!r("call +91-98765-43210 tomorrow").contains("98765"));
        assert!(!r("hash 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08 stored").contains("9f86d0"));
        assert!(!r("password: hunter2").contains("hunter2"));
    }

    #[test]
    fn a_zone_name_is_not_mistaken_for_an_aws_key_but_a_real_key_is_removed() {
        assert_eq!(r("use a name such as Asia/Kolkata"), "use a name such as Asia/Kolkata");
        assert!(r("key AKIAIOSFODNN7EXAMPLE here").contains("[redacted]"));
    }

    #[test]
    fn ordinary_engineering_text_is_untouched() {
        let s = "Open the Google search page, read the Nifty 50 level 22716.20 and quote it; verify against the snapshot.";
        assert_eq!(redact(s), (s.to_string(), 0));
        let t = "Add validation in src/service/orders.rs then run cargo test --lib";
        assert_eq!(redact(t).1, 0);
        // "session" and "auth" only count as keys when followed by a value.
        assert_eq!(redact("the session store and auth service are separate").1, 0);
    }

    #[test]
    fn a_credential_dump_is_refused_not_stored() {
        assert!(clean_for_storage("password=a token=b secret=c apikey=d").is_none());
        assert!(clean_for_storage("").is_none());
        assert!(clean_for_storage("Read the page, quote the figures, verify them with password=hunter2 not needed").is_some());
    }
}

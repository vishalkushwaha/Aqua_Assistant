//! Append-only record of every browser call (`data/browser/audit.jsonl`), rotated at 10 MB.
//!
//! What it keeps is what was done, not what was typed or where a page's query string led: typed text is replaced by its length, and a web address is cut to its origin and
//! path (no user name, no query, no fragment), because both are the person's: a search, a message, a link with a token in it (card R6-12). `AQUA_BROWSER_AUDIT_FULL=on`
//! keeps them whole, except for text typed into a field that was marked sensitive, which is never kept.

use serde::Serialize;
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

const ROTATE_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry<'a> {
    pub ts_unix_ms: u64,
    pub backend: &'a str,
    pub tool: &'a str,
    pub args: Value,
    pub url_before: Option<&'a str>,
    pub url_after: Option<&'a str>,
    /// `allowed`, `confirmed`, `denied`, or `blocked`.
    pub decision: &'a str,
    pub ok: bool,
    pub error_code: Option<&'a str>,
    pub duration_ms: u64,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub struct AuditLog {
    path: PathBuf,
    lock: Mutex<()>,
}

impl AuditLog {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    /// Best effort: auditing must never break a browser action.
    pub fn append(&self, entry: &AuditEntry<'_>) {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() > ROTATE_BYTES) {
            let _ = std::fs::rename(&self.path, self.path.with_extension("1.jsonl"));
        }
        let Ok(mut value) = serde_json::to_value(entry) else { return };
        if !keep_full() {
            for key in ["url_before", "url_after"] {
                if let Some(url) = value.get(key).and_then(|v| v.as_str()).map(reduce_url) {
                    value[key] = Value::String(url);
                }
            }
        }
        let Ok(line) = serde_json::to_string(&value) else { return };
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// Does the person want the audit to keep typed text and whole web addresses? Off unless `AQUA_BROWSER_AUDIT_FULL` says on.
fn keep_full() -> bool {
    keep_full_from(std::env::var("AQUA_BROWSER_AUDIT_FULL").ok().as_deref())
}

fn keep_full_from(value: Option<&str>) -> bool {
    matches!(value.map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("on" | "1" | "true" | "yes"))
}

/// A web address as the audit keeps it: its origin and path, with no user name or password, no query and no fragment. An address of another kind (`data:`, `javascript:`)
/// keeps its scheme and its length.
pub fn reduce_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) if matches!(u.scheme(), "http" | "https" | "ws" | "wss" | "ftp" | "file") => {
            let host = u.host_str().map(|h| format!("{h}{}", u.port().map(|p| format!(":{p}")).unwrap_or_default())).unwrap_or_default();
            format!("{}://{host}{}", u.scheme(), u.path())
        }
        Ok(u) => format!("{}:<{} chars>", u.scheme(), url.chars().count().saturating_sub(u.scheme().len() + 1)),
        // Not an address that can be read: cut where a query or a fragment would begin, and take a user name off.
        Err(_) => {
            let cut = url.split(['?', '#']).next().unwrap_or("");
            match cut.split_once("://") {
                Some((scheme, rest)) => format!("{scheme}://{}", rest.rsplit_once('@').map(|(_, host)| host).unwrap_or(rest)),
                None => cut.to_string(),
            }
        }
    }
}

/// The arguments of a call as the audit keeps them (card R6-12): typed text is replaced by its length, and a web address is cut to its origin and path. Text typed into a
/// field marked `sensitive` (a credential or a payment) is replaced even when the person asked for the full audit.
pub fn redact_args(tool: &str, args: &Value, sensitive: bool) -> Value {
    redact_args_with(tool, args, sensitive, keep_full())
}

fn redact_args_with(tool: &str, args: &Value, sensitive: bool, keep_full: bool) -> Value {
    let mut a = args.clone();
    let redact = |v: &mut Value| {
        if let Some(s) = v.as_str() {
            *v = Value::String(format!("<redacted {} chars>", s.chars().count()));
        }
    };
    let hide_text = sensitive || !keep_full;
    match tool {
        "browser_type" if hide_text => {
            if let Some(t) = a.get_mut("text") {
                redact(t);
            }
        }
        "browser_fill_form" if hide_text => {
            if let Some(fields) = a.get_mut("fields").and_then(|f| f.as_array_mut()) {
                for f in fields {
                    if let Some(v) = f.get_mut("value") {
                        redact(v);
                    }
                }
            }
        }
        _ => {}
    }
    if !keep_full {
        if let Some(url) = a.get("url").and_then(|v| v.as_str()).map(reduce_url) {
            a["url"] = Value::String(url);
        }
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Card R6-12: typed text was redacted only for a field marked sensitive, and every address was kept whole, with its query and any user name in it.
    #[test]
    fn typed_text_and_the_query_of_an_address_are_not_kept_unless_the_person_asks() {
        let typed = json!({"target": "f1e6", "text": "my private message"});
        let form = json!({"fields": [{"name": "who", "ref": "e1", "value": "Asha"}, {"name": "pw", "ref": "e2", "value": "hunter2"}]});
        let nav = json!({"url": "https://user:pass@shop.example:8443/cart/pay?card=4111111111111111&token=abc#top"});
        // By default: text becomes its length, the address its origin and path.
        assert_eq!(redact_args_with("browser_type", &typed, false, false)["text"], "<redacted 18 chars>");
        let f = redact_args_with("browser_fill_form", &form, false, false);
        assert_eq!((f["fields"][0]["value"].as_str(), f["fields"][1]["value"].as_str()), (Some("<redacted 4 chars>"), Some("<redacted 7 chars>")));
        assert_eq!(redact_args_with("browser_navigate", &nav, false, false)["url"], "https://shop.example:8443/cart/pay");
        // Asked for in full: kept, except where the field is marked sensitive.
        assert_eq!(redact_args_with("browser_type", &typed, false, true)["text"], "my private message");
        assert_eq!(redact_args_with("browser_type", &typed, true, true)["text"], "<redacted 18 chars>");
        assert_eq!(redact_args_with("browser_navigate", &nav, false, true)["url"], nav["url"]);
        // The switch.
        for on in ["on", "1", "TRUE", " yes "] {
            assert!(keep_full_from(Some(on)), "{on}");
        }
        for off in [None, Some(""), Some("off"), Some("0"), Some("maybe")] {
            assert!(!keep_full_from(off), "{off:?}");
        }
    }

    #[test]
    fn an_address_is_cut_to_its_origin_and_path_whatever_it_is() {
        for (from, to) in [
            ("https://example.com/a/b?q=1#x", "https://example.com/a/b"),
            ("http://user:secret@127.0.0.1:8791/form.html?next=/admin", "http://127.0.0.1:8791/form.html"),
            ("https://example.com", "https://example.com/"),
            ("about:blank", "about:<5 chars>"),
            ("data:text/html,<h1>hi</h1>", "data:<21 chars>"),
            ("javascript:alert(1)", "javascript:<8 chars>"),
        ] {
            assert_eq!(reduce_url(from), to, "{from}");
        }
        // Something that cannot be read as an address is still cut where a query would begin, and loses a user name.
        assert_eq!(reduce_url("not a url?secret=1"), "not a url");
        assert_eq!(reduce_url("http://me:pw@[bad/p?q=1#f"), "http://[bad/p");
    }

    #[test]
    fn the_log_keeps_the_origin_and_path_of_the_pages_and_not_their_queries() {
        let dir = std::env::temp_dir().join(format!("aqua_audit_url_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = AuditLog::new(dir.join("audit.jsonl"));
        log.append(&AuditEntry {
            ts_unix_ms: now_ms(),
            backend: "managed",
            tool: "browser_click",
            args: json!({}),
            url_before: Some("https://mail.example/inbox?token=SECRETVALUE"),
            url_after: Some("https://mail.example/message/7?session=ANOTHERSECRET#reply"),
            decision: "allowed",
            ok: true,
            error_code: None,
            duration_ms: 5,
        });
        let text = std::fs::read_to_string(dir.join("audit.jsonl")).unwrap();
        assert!(text.contains("https://mail.example/inbox") && text.contains("https://mail.example/message/7"), "{text}");
        assert!(!text.contains("SECRETVALUE") && !text.contains("ANOTHERSECRET") && !text.contains("#reply"), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn writes_lines_and_redacts_sensitive_text() {
        let dir = std::env::temp_dir().join(format!("aqua_audit_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = AuditLog::new(dir.join("audit.jsonl"));
        let args = redact_args("browser_type", &json!({"target": "f1e6", "text": "hunter2"}), true);
        log.append(&AuditEntry {
            ts_unix_ms: now_ms(),
            backend: "managed",
            tool: "browser_type",
            args,
            url_before: Some("http://127.0.0.1:8791/form.html"),
            url_after: None,
            decision: "confirmed",
            ok: true,
            error_code: None,
            duration_ms: 12,
        });
        let text = std::fs::read_to_string(dir.join("audit.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("<redacted 7 chars>"));
        assert!(!text.contains("hunter2"));
        let _ = std::fs::remove_dir_all(dir);
    }
}

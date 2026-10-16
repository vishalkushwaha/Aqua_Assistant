//! Naming a failure. Rules over the error text, most specific first; anything unrecognised is `Unknown`,
//! never guessed into a class that would trigger the wrong recovery.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Authentication,
    Network,
    Timeout,
    RateLimit,
    InvalidInput,
    MissingFile,
    BrowserError,
    WebsiteUiChange,
    ApiError,
    PermissionError,
    ExternalService,
    Unknown,
}

impl ErrorClass {
    /// The snake_case name used in stored records.
    pub fn key(self) -> &'static str {
        match self {
            ErrorClass::Authentication => "authentication",
            ErrorClass::Network => "network",
            ErrorClass::Timeout => "timeout",
            ErrorClass::RateLimit => "rate_limit",
            ErrorClass::InvalidInput => "invalid_input",
            ErrorClass::MissingFile => "missing_file",
            ErrorClass::BrowserError => "browser_error",
            ErrorClass::WebsiteUiChange => "website_ui_change",
            ErrorClass::ApiError => "api_error",
            ErrorClass::PermissionError => "permission_error",
            ErrorClass::ExternalService => "external_service",
            ErrorClass::Unknown => "unknown",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ErrorClass::Authentication => "Authentication",
            ErrorClass::Network => "Network",
            ErrorClass::Timeout => "Timeout",
            ErrorClass::RateLimit => "Rate limit",
            ErrorClass::InvalidInput => "Invalid input",
            ErrorClass::MissingFile => "Missing file",
            ErrorClass::BrowserError => "Browser error",
            ErrorClass::WebsiteUiChange => "Website change",
            ErrorClass::ApiError => "API error",
            ErrorClass::PermissionError => "Permission",
            ErrorClass::ExternalService => "External service",
            ErrorClass::Unknown => "Unknown",
        }
    }

    /// What it usually means, in a sentence for a person.
    pub fn explain(self) -> &'static str {
        match self {
            ErrorClass::Authentication => "the account was not accepted (a login expired or a key is wrong)",
            ErrorClass::Network => "the connection failed",
            ErrorClass::Timeout => "it took too long to answer",
            ErrorClass::RateLimit => "the service asked Aqua to slow down",
            ErrorClass::InvalidInput => "a setting or a value was not valid",
            ErrorClass::MissingFile => "a file it needs is not there",
            ErrorClass::BrowserError => "the browser had a problem",
            ErrorClass::WebsiteUiChange => "the page no longer looks the way the step expects",
            ErrorClass::ApiError => "the service answered with an error",
            ErrorClass::PermissionError => "Aqua is not allowed to do that",
            ErrorClass::ExternalService => "the outside service is having trouble",
            ErrorClass::Unknown => "the cause is not clear from the message",
        }
    }

    /// May a plain retry help? (A wrong password will not be right the second time.)
    pub fn transient(self) -> bool {
        matches!(self, ErrorClass::Network | ErrorClass::Timeout | ErrorClass::RateLimit | ErrorClass::ExternalService | ErrorClass::BrowserError)
    }
}

/// Whole-word-ish HTTP status match: "http 429", "status 429", "429 too many", not "port 14290".
fn status(t: &str, code: &str) -> bool {
    t.match_indices(code).any(|(i, _)| {
        let before = t[..i].chars().last();
        let after = t[i + code.len()..].chars().next();
        !before.is_some_and(|c| c.is_ascii_alphanumeric()) && !after.is_some_and(|c| c.is_ascii_alphanumeric())
    })
}

pub fn classify(message: &str) -> ErrorClass {
    let t = message.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| t.contains(w));

    if status(&t, "429") || has(&["rate limit", "rate-limit", "too many requests", "quota exceeded", "slow down"]) {
        return ErrorClass::RateLimit;
    }
    if status(&t, "401") || has(&["unauthorized", "unauthorised", "not logged in", "log in again", "login required", "sign in", "session expired", "session has expired", "invalid token", "token expired", "authentication", "invalid credentials", "invalid api key", "captcha"]) {
        return ErrorClass::Authentication;
    }
    if status(&t, "403") || has(&["forbidden", "permission denied", "access denied", "not permitted", "not allowed", "refused by policy", "blocked by policy", "blocked by the browser policy"]) {
        return ErrorClass::PermissionError;
    }
    if has(&["timed out", "timeout", "time out", "deadline exceeded", "took too long"]) {
        return ErrorClass::Timeout;
    }
    if has(&["selector", "element not found", "no element", "could not find the button", "could not find the field", "could not find the element", "no longer on the page", "page layout", "unexpected page"]) {
        return ErrorClass::WebsiteUiChange;
    }
    if has(&["browser had a problem", "browser stopped", "browser crashed", "browser is not connected", "browser did not answer", "browser is not running", "devtools", "cdp ", "page crashed", "target closed", "edge could not"]) {
        return ErrorClass::BrowserError;
    }
    if has(&["no such file", "file not found", "cannot find the file", "os error 2", "path does not exist", "the system cannot find the path", "is not a file"]) || (t.contains("not found") && (t.contains("file") || t.contains("folder") || t.contains("path"))) {
        return ErrorClass::MissingFile;
    }
    if has(&["connection refused", "connection reset", "connection closed", "connection error", "could not connect", "dns", "network", "unreachable", "offline", "no route", "tls", "certificate", "error sending request"]) {
        return ErrorClass::Network;
    }
    if status(&t, "502") || status(&t, "503") || status(&t, "504") || has(&["service unavailable", "bad gateway", "gateway timeout", "upstream", "temporarily unavailable", "overloaded"]) {
        return ErrorClass::ExternalService;
    }
    if status(&t, "500") || status(&t, "400") || status(&t, "404") || status(&t, "422") || has(&["api error", "http error", "status code"]) {
        return ErrorClass::ApiError;
    }
    if has(&["missing:", "is required", "not valid", "invalid", "bad setting", "cannot be empty", "cannot parse", "could not parse", "malformed", "unknown variable", "no value for"]) {
        return ErrorClass::InvalidInput;
    }
    ErrorClass::Unknown
}

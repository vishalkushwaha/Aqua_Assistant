//! Phase 4: the browser steps ("Open website" and "Browser action").
//!
//! These are deterministic steps, not the agent: a step names an action and an element, and this finds the
//! element on the page by its role and visible name (never by position or a stored selector), does the one
//! thing, and reports what happened. Every call goes through the guarded browser backend, so the site policy,
//! the approvals for irreversible buttons and password fields, the upload rules and the audit log are the same
//! as for the agent. An unattended run cannot answer an approval, so it is refused, not waited for.
//!
//! What a failure means is decided here, because it decides what may be retried:
//! - reading, waiting, opening a page: a timeout or a crashed browser is worth another try;
//! - clicking, typing, choosing, uploading: after a timeout it is not known whether it happened, so the step
//!   stops as "unknown" and a person checks. It is never repeated on a guess.
//! - an element that is not on the page is reported with what *is* there, so a person (or, later, a recovery)
//!   can see how the page changed.

use crate::flows::model::{Node, NodeKind};
use crate::flows::run::exec::{NodeCtx, NodeError, NodeExecutor, NodeResult};
use aqua_mcp_adapters::browser::{BrowserBackend, BrowserErrorCode, BrowserObservation};
use serde_json::{json, Map, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// What the executor needs from a browser. The real one is the guarded backend; tests use a page in memory.
pub trait Driver: Send + Sync {
    fn call(&self, tool: &str, args: Value) -> BrowserObservation;
}

impl Driver for BrowserBackend {
    fn call(&self, tool: &str, args: Value) -> BrowserObservation {
        BrowserBackend::call(self, tool, args)
    }
}

pub struct BrowserNodeExecutor {
    driver: Arc<dyn Driver>,
}

impl BrowserNodeExecutor {
    pub fn new(driver: Arc<dyn Driver>) -> Arc<Self> {
        Arc::new(Self { driver })
    }
}

pub(crate) fn text_of(cfg: &Value, key: &str) -> String {
    cfg.get(key).and_then(|v| v.as_str()).unwrap_or("").trim().to_string()
}

fn flag(cfg: &Value, key: &str) -> bool {
    match cfg.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.trim().to_lowercase().as_str(), "true" | "yes" | "1"),
        _ => false,
    }
}

// ---- finding an element -------------------------------------------------------------------------------------------------

/// One element on the page, read from a snapshot line such as `- button "Send" [ref=f1e11]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Element {
    pub role: String,
    pub name: String,
    pub target: String,
}

/// Every element with a name and a reference in some snapshot text.
pub fn elements(snapshot: &str) -> Vec<Element> {
    let mut out = Vec::new();
    for line in snapshot.lines() {
        let l = line.trim_start().trim_start_matches("- ");
        let Some(open) = l.find("[ref=") else { continue };
        let Some(close) = l[open..].find(']') else { continue };
        let target = l[open + 5..open + close].to_string();
        let head = l[..open].trim();
        let (role, name) = match head.split_once(' ') {
            Some((r, rest)) => {
                let name = rest.trim();
                match (name.find('"'), name.rfind('"')) {
                    (Some(a), Some(b)) if b > a => (r.to_string(), name[a + 1..b].to_string()),
                    _ => (r.to_string(), String::new()),
                }
            }
            None => (head.to_string(), String::new()),
        };
        if !name.is_empty() {
            out.push(Element { role: role.trim_end_matches(':').to_string(), name, target });
        }
    }
    out
}

/// "Send", "button:Send", "textbox: Email" -> (role, name).
fn parse_target(spec: &str) -> (Option<String>, String) {
    let s = spec.trim();
    if let Some((r, n)) = s.split_once(':') {
        let r = r.trim().to_lowercase();
        if !r.is_empty() && r.chars().all(|c| c.is_ascii_alphabetic()) && r.len() <= 12 {
            return (Some(r), n.trim().to_string());
        }
    }
    (None, s.to_string())
}

pub enum Found {
    One(Element),
    /// More than one element fits equally well: guessing could click the wrong thing.
    Ambiguous(Vec<Element>),
    /// Nothing fits. What is on the page instead, so the change can be seen.
    Missing(Vec<Element>),
}

/// Best match for `spec` among `page`: role (if given) must fit; an exact name beats one that only contains it.
pub fn pick(spec: &str, page: &[Element]) -> Found {
    let (role, name) = parse_target(spec);
    let want = name.to_lowercase();
    let role_ok = |e: &Element| role.as_ref().is_none_or(|r| e.role.eq_ignore_ascii_case(r));
    let exact: Vec<&Element> = page.iter().filter(|e| role_ok(e) && e.name.to_lowercase() == want).collect();
    let pool: Vec<&Element> = if exact.is_empty() { page.iter().filter(|e| role_ok(e) && !want.is_empty() && e.name.to_lowercase().contains(&want)).collect() } else { exact };
    let mut unique: Vec<Element> = Vec::new();
    for e in pool {
        if !unique.iter().any(|u| u.target == e.target) {
            unique.push(e.clone());
        }
    }
    match unique.len() {
        1 => Found::One(unique.remove(0)),
        0 => {
            // Offer the elements a person might have meant: same role, or sharing a word.
            let words: Vec<&str> = want.split_whitespace().collect();
            let near: Vec<Element> = page.iter().filter(|e| role_ok(e) || words.iter().any(|w| e.name.to_lowercase().contains(w))).take(8).cloned().collect();
            Found::Missing(near)
        }
        _ => Found::Ambiguous(unique.into_iter().take(6).collect()),
    }
}

pub(crate) fn describe(list: &[Element]) -> String {
    list.iter().map(|e| format!("{} \"{}\"", e.role, e.name)).collect::<Vec<_>>().join(", ")
}

// ---- errors ------------------------------------------------------------------------------------------------------------------

/// Does the action change something on the page (so that after a timeout it may or may not have happened)?
fn mutating(action: &str) -> bool {
    matches!(action, "click" | "type" | "select" | "upload")
}

pub(crate) fn failed(o: &BrowserObservation, action: &str, what: &str) -> NodeError {
    let (code, msg) = o.error.as_ref().map(|e| (e.code, e.message.clone())).unwrap_or((BrowserErrorCode::BackendError, "the browser reported a problem".into()));
    match code {
        BrowserErrorCode::ConfirmationDenied | BrowserErrorCode::ConfirmationRequired => NodeError::Fatal(format!("{what}: the action was not allowed (approval was denied or nobody was there to give it): {msg}")),
        BrowserErrorCode::PolicyBlockedOrigin | BrowserErrorCode::PolicyToolBlocked | BrowserErrorCode::PolicyJsDisabled | BrowserErrorCode::PolicyUploadPath => NodeError::Fatal(format!("{what}: blocked by the browser policy: {msg}")),
        BrowserErrorCode::InvalidArguments => NodeError::Fatal(format!("{what}: invalid setting: {msg}")),
        BrowserErrorCode::ElementNotFound | BrowserErrorCode::StaleRef => NodeError::Fatal(format!("{what}: element not found on the page: {msg}")),
        BrowserErrorCode::DialogBlocking => NodeError::Fatal(format!("{what}: a dialog is blocking the page: {msg}")),
        BrowserErrorCode::TabLimit => NodeError::Fatal(format!("{what}: too many tabs are open: {msg}")),
        BrowserErrorCode::ExtensionNotConnected => NodeError::Fatal(format!("{what}: the browser is not connected: {msg}")),
        // The page may have changed before the answer was lost.
        BrowserErrorCode::Timeout if mutating(action) => NodeError::Unknown(format!("{what}: the browser did not answer in time, so it is not known whether it happened")),
        BrowserErrorCode::BrowserCrashed | BrowserErrorCode::BrowserNotRunning if mutating(action) => NodeError::Unknown(format!("{what}: the browser stopped, so it is not known whether it happened")),
        BrowserErrorCode::Timeout => NodeError::Retryable(format!("{what}: timed out: {msg}")),
        BrowserErrorCode::BrowserCrashed | BrowserErrorCode::BrowserNotRunning => NodeError::Retryable(format!("{what}: the browser had a problem (browser stopped): {msg}")),
        BrowserErrorCode::NavigationFailed => NodeError::Retryable(format!("{what}: navigation failed (could not connect): {msg}")),
        BrowserErrorCode::BackendError => NodeError::Retryable(format!("{what}: the browser had a problem: {msg}")),
    }
}

pub(crate) fn cancelled(ctx: &NodeCtx) -> Result<(), NodeError> {
    if ctx.cancel.load(Ordering::SeqCst) {
        Err(NodeError::Fatal("stopped".into()))
    } else {
        Ok(())
    }
}

// ---- the steps ---------------------------------------------------------------------------------------------------------------

/// A page that shows a sign-in form, not the content behind it.
pub(crate) fn looks_signed_out(o: &BrowserObservation, snapshot: &str) -> bool {
    let s = snapshot.to_lowercase();
    let url = o.url.clone().unwrap_or_default().to_lowercase();
    let form = s.contains("textbox \"password\"") || s.contains("password\" [ref") || s.contains("type=\"password\"");
    let words = ["sign in", "log in", "login", "sign-in"].iter().any(|w| s.contains(w));
    let path = ["/login", "/signin", "/sign-in", "/checkpoint", "/authwall", "/uas/login"].iter().any(|p| url.contains(p));
    path || (form && words)
}

impl BrowserNodeExecutor {
    fn open(&self, ctx: &NodeCtx) -> Result<NodeResult, NodeError> {
        let cfg = &ctx.config;
        let url = text_of(cfg, "url");
        if url.is_empty() {
            return Err(NodeError::Fatal("invalid setting: the website address is empty".into()));
        }
        let nav = self.driver.call("browser_navigate", json!({ "url": url }));
        if !nav.ok {
            return Err(failed(&nav, "open", &format!("opening {url}")));
        }
        cancelled(ctx)?;
        let snap = self.driver.call("browser_snapshot", json!({}));
        let snapshot = if snap.ok { snap.text.clone() } else { String::new() };
        let logged_in = !looks_signed_out(&snap, &snapshot);
        let title = nav.title.clone().or(snap.title.clone()).unwrap_or_default();
        let at = nav.url.clone().or(snap.url.clone()).unwrap_or(url.clone());
        if flag(cfg, "require_login") && !logged_in {
            return Err(NodeError::Fatal(format!("not logged in at {at}: sign in there in the Aqua browser (Aqua never asks for or stores the password), then run this step again")));
        }
        let mut o = Map::new();
        o.insert("page".into(), json!(format!("{title} ({at})")));
        o.insert("logged_in".into(), json!(logged_in));
        Ok(NodeResult { outputs: o, logs: vec![format!("Opened {at}"), if logged_in { "Signed in (no sign-in form seen)".into() } else { "A sign-in form is showing: not signed in".into() }] })
    }

    /// Find the element the step names on the current page. Looks it up by its visible name, so a page that
    /// moved things around still works; a page that renamed them does not, and says so.
    fn resolve(&self, spec: &str, action: &str) -> Result<Element, NodeError> {
        let (_, name) = parse_target(spec);
        let mut page = Vec::new();
        // The small search first (cheap), then the whole page, so a long page is not cut off before the element.
        for (tool, args) in [("browser_find", json!({ "text": name })), ("browser_snapshot", json!({}))] {
            let o = self.driver.call(tool, args);
            if !o.ok {
                if tool == "browser_snapshot" {
                    return Err(failed(&o, action, &format!("looking for \"{spec}\"")));
                }
                continue;
            }
            page = elements(&o.text);
            match pick(spec, &page) {
                Found::One(e) => return Ok(e),
                Found::Ambiguous(list) => return Err(NodeError::Fatal(format!("invalid setting: more than one element matches \"{spec}\" ({}). Give the role and the full name, for example button:Send", describe(&list)))),
                Found::Missing(_) => {}
            }
        }
        let near = match pick(spec, &page) {
            Found::Missing(n) => n,
            _ => vec![],
        };
        Err(NodeError::Fatal(format!("element not found on the page: \"{spec}\"{}", if near.is_empty() { ". The page shows nothing like it".to_string() } else { format!(". Similar elements on the page: {}", describe(&near)) })))
    }

    fn act(&self, ctx: &NodeCtx, node: &Node) -> Result<NodeResult, NodeError> {
        let cfg = &ctx.config;
        let action = text_of(cfg, "action");
        let action = if action.is_empty() { "click".to_string() } else { action };
        let target = text_of(cfg, "target");
        let value = cfg.get("value").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let mut logs = Vec::new();
        let mut o = Map::new();
        let need_target = |a: &str| -> Result<(), NodeError> {
            if target.is_empty() {
                Err(NodeError::Fatal(format!("invalid setting: \"{a}\" needs an element (role and name, such as button:Send)")))
            } else {
                Ok(())
            }
        };
        match action.as_str() {
            "click" => {
                need_target("click")?;
                let e = self.resolve(&target, &action)?;
                cancelled(ctx)?;
                let r = self.driver.call("browser_click", json!({ "target": e.target, "element": e.name }));
                if !r.ok {
                    return Err(failed(&r, &action, &format!("clicking {} \"{}\"", e.role, e.name)));
                }
                logs.push(format!("Clicked {} \"{}\"", e.role, e.name));
                o.insert("result".into(), json!(format!("clicked {} \"{}\"", e.role, e.name)));
            }
            "type" => {
                need_target("type")?;
                let e = self.resolve(&target, &action)?;
                cancelled(ctx)?;
                let r = self.driver.call("browser_type", json!({ "target": e.target, "text": value, "element": e.name }));
                if !r.ok {
                    return Err(failed(&r, &action, &format!("typing into {} \"{}\"", e.role, e.name)));
                }
                // What was typed is not logged: it may be private.
                logs.push(format!("Typed {} characters into {} \"{}\"", value.chars().count(), e.role, e.name));
                o.insert("result".into(), json!(format!("typed into {} \"{}\"", e.role, e.name)));
            }
            "select" => {
                need_target("select")?;
                let e = self.resolve(&target, &action)?;
                cancelled(ctx)?;
                let r = self.driver.call("browser_select_option", json!({ "target": e.target, "values": [value], "element": e.name }));
                if !r.ok {
                    return Err(failed(&r, &action, &format!("choosing in {} \"{}\"", e.role, e.name)));
                }
                logs.push(format!("Chose \"{value}\" in {} \"{}\"", e.role, e.name));
                o.insert("result".into(), json!(format!("chose {value}")));
            }
            "upload" => {
                if value.is_empty() {
                    return Err(NodeError::Fatal("invalid setting: \"upload\" needs the file path in Value (files must be in Aqua's uploads folder)".into()));
                }
                if !target.is_empty() {
                    // Opening the file chooser is a click.
                    let e = self.resolve(&target, &action)?;
                    let r = self.driver.call("browser_click", json!({ "target": e.target, "element": e.name }));
                    if !r.ok {
                        return Err(failed(&r, &action, &format!("opening the file chooser at {} \"{}\"", e.role, e.name)));
                    }
                }
                cancelled(ctx)?;
                let r = self.driver.call("browser_file_upload", json!({ "paths": [value] }));
                if !r.ok {
                    return Err(failed(&r, &action, "uploading the file"));
                }
                logs.push(format!("Uploaded {value}"));
                o.insert("result".into(), json!(format!("uploaded {value}")));
            }
            "read" => {
                let text = if target.is_empty() {
                    let a = self.driver.call("aqua_browser_extract", json!({ "kind": "article" }));
                    if a.ok && !a.text.trim().is_empty() {
                        a.text
                    } else {
                        let s = self.driver.call("browser_snapshot", json!({}));
                        if !s.ok {
                            return Err(failed(&s, &action, "reading the page"));
                        }
                        s.text
                    }
                } else {
                    let e = self.resolve(&target, &action)?;
                    e.name
                };
                let text: String = text.chars().take(6000).collect();
                logs.push(format!("Read {} characters", text.chars().count()));
                o.insert("result".into(), json!(text));
            }
            "wait_for" => {
                let seconds = value.trim().parse::<u64>().ok();
                let args = match seconds {
                    Some(s) => json!({ "time": s.min(30) }),
                    None if !value.is_empty() => json!({ "text": value }),
                    None if !target.is_empty() => json!({ "text": parse_target(&target).1 }),
                    None => return Err(NodeError::Fatal("invalid setting: \"wait_for\" needs text to wait for (in Value) or a number of seconds".into())),
                };
                let r = self.driver.call("browser_wait_for", args);
                if !r.ok {
                    return Err(failed(&r, &action, "waiting"));
                }
                logs.push("Waited".into());
                o.insert("result".into(), json!("done waiting"));
            }
            "screenshot" => {
                let r = self.driver.call("browser_take_screenshot", json!({}));
                if !r.ok {
                    return Err(failed(&r, &action, "taking a screenshot"));
                }
                let path = r.artifacts.first().map(|a| a.path.clone()).unwrap_or_default();
                logs.push(format!("Screenshot saved: {path}"));
                o.insert("result".into(), json!(path));
            }
            other => return Err(NodeError::Fatal(format!("invalid setting: the browser action \"{other}\" is not supported (click, type, select, upload, read, wait_for, screenshot)"))),
        }
        let _ = node;
        Ok(NodeResult { outputs: o, logs })
    }
}

impl NodeExecutor for BrowserNodeExecutor {
    fn kinds(&self) -> &[NodeKind] {
        &[NodeKind::Browser, NodeKind::Website]
    }

    fn execute(&self, ctx: &NodeCtx) -> Result<NodeResult, NodeError> {
        cancelled(ctx)?;
        match ctx.node.kind {
            NodeKind::Website => self.open(ctx),
            _ => self.act(ctx, ctx.node),
        }
    }
}

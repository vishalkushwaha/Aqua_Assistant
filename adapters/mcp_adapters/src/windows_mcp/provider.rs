//! Windows desktop UI tools as a [`ToolProvider`] for the tool host (`win.*`).
//!
//! The provider adds no policy of its own: it asks the same rules the guard asks and turns the answer into permission classes, and the host decides. Since card R6-08 it asks **all**
//! of them, so that the host is never looser than the guard (`WindowsGuard::call_at`) was:
//!
//! * the guard's window rules and the tier of the window in front ([`evaluate_tiered`]), as the caller (the voice is judged as the voice, so a spoken request types text into the
//!   window in front and does nothing else that acts: decision D-14);
//! * a sign-in or payment step, found from the title of the window or from what the last snapshot of it showed ([`critical`]): asked about whatever the settings are, refused to the voice;
//! * one session at a time ([`session`]): a call that acts on the screen while another session has the desktop is refused before anyone is asked;
//! * and the window in front is read again when the call is made, after any question (the person may have taken a minute): a different program or window in front means nothing is sent.
//!
//! Arguments are validated against a schema that leaves out `App`'s `executable`, `args` and `cwd`, so the host rejects them before the guard even sees them.

use super::guard::{
    caller_backend, describe, evaluate_tiered, foreground_window, forward, input_target_changed, is_observe_call, is_observe_tool, presses_enter, shell_or_admin_reason, Caller as GuardCaller,
    DenyKind, Foreground, ForwardError, Screen, Verdict,
};
use super::{critical, session, tiers};
use crate::stdio_mcp::StdioMcpClient;
use aqua_tool_host::provider::ClassifyContext;
use aqua_tool_host::{
    Annotations, Caller, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

/// (host name, server name, one-line description, input schema properties, extra keys allowed)
type Spec = (&'static str, &'static str, &'static str, Value, bool);

fn specs() -> Vec<Spec> {
    let loc = json!({"type": "array", "items": {"type": "integer"}, "minItems": 2, "maxItems": 2});
    vec![
        ("win.app", "App", "Launch, switch to or resize a desktop app by name (app name only, no commands).",
            json!({"mode": {"type": "string"}, "name": {"type": "string", "maxLength": 200}, "window_loc": loc.clone(), "window_size": loc.clone()}), false),
        ("win.snapshot", "Snapshot", "Read the screen: window list and the interactive elements with their labels.",
            json!({"use_vision": {"type": "boolean"}, "use_dom": {"type": "boolean"}, "use_annotation": {"type": "boolean"}, "use_ui_tree": {"type": "boolean"}, "window_metadata_only": {"type": "boolean"}, "display": {"type": "array", "items": {"type": "integer"}}}), false),
        ("win.screenshot", "Screenshot", "Take a screenshot of the desktop.",
            json!({"use_annotation": {"type": "boolean"}, "display": {"type": "array", "items": {"type": "integer"}}}), false),
        ("win.display_inventory", "DisplayInventory", "List the connected displays.", json!({}), false),
        ("win.click", "Click", "Click at screen coordinates loc=[x,y] or at an element label from the last snapshot.",
            json!({"loc": loc.clone(), "label": {"type": "integer"}, "button": {"type": "string"}, "clicks": {"type": "integer", "minimum": 1, "maximum": 3}}), false),
        ("win.type", "Type", "Type text into the element at loc or label; set press_enter to submit.",
            json!({"text": {"type": "string", "maxLength": 4000}, "loc": loc.clone(), "label": {"type": "integer"}, "clear": {"type": "boolean"}, "caret_position": {"type": "string"}, "press_enter": {"type": "boolean"}}), false),
        ("win.scroll", "Scroll", "Scroll the window or the element at loc or label.",
            json!({"loc": loc.clone(), "label": {"type": "integer"}, "type": {"type": "string"}, "direction": {"type": "string"}, "wheel_times": {"type": "integer", "minimum": 1, "maximum": 50}}), false),
        ("win.move", "Move", "Move the pointer, or drag from from_loc to loc.",
            json!({"loc": loc.clone(), "label": {"type": "integer"}, "drag": {"type": "boolean"}, "from_loc": loc.clone(), "duration": {"type": "number"}}), false),
        ("win.shortcut", "Shortcut", "Press a keyboard shortcut such as ctrl+s.",
            json!({"shortcut": {"type": "string", "minLength": 1, "maxLength": 64}}), false),
        ("win.wait", "Wait", "Wait a number of seconds.", json!({"duration": {"type": "number", "minimum": 0, "maximum": 60}}), false),
        ("win.wait_for", "WaitFor", "Wait until text or a window appears.",
            json!({"condition": {"type": "string"}, "text": {"type": "string", "maxLength": 500}, "window_name": {"type": "string", "maxLength": 200}, "timeout": {"type": "number", "minimum": 0, "maximum": 120}, "interval": {"type": "number"}, "use_dom": {"type": "boolean"}}), false),
        ("win.multi_select", "MultiSelect", "Select several elements, by locations or labels.",
            json!({"locs": {"type": "array"}, "labels": {"type": "array"}, "press_ctrl": {"type": "boolean"}}), false),
        ("win.multi_edit", "MultiEdit", "Fill several fields in one step.",
            json!({"locs": {"type": "array"}, "labels": {"type": "array"}}), true),
        ("win.notification", "Notification", "Show a desktop notification.",
            json!({"title": {"type": "string", "maxLength": 200}, "message": {"type": "string", "maxLength": 1000}, "app_id": {"type": "string"}}), false),
    ]
}

/// The host's name for a Windows-MCP tool name (`Click` is `win.click`), for the calls that do not go through the host yet.
pub fn host_name_for_wire(wire: &str) -> Option<&'static str> {
    specs().into_iter().find(|s| s.1 == wire).map(|s| s.0)
}

/// The host's name for a desktop tool as the model writes it, in any case (`click` is `win.click`): the guard reads the names that way (`WindowsGuard::call`), so the door does too, and
/// a change of case is not a way round it. A name with anything else about it (a space) is not a tool, as it is not for the guard.
pub fn host_name_for_model_name(name: &str) -> Option<&'static str> {
    specs().into_iter().find(|s| s.1.eq_ignore_ascii_case(name)).map(|s| s.0)
}

/// What the provider knows about the desktop at the moment it judges a call (card R6-08).
#[derive(Debug, Clone, Copy)]
pub struct Desktop<'a> {
    /// The window in front; `None` when it cannot be read.
    pub front: Option<&'a Foreground>,
    pub full_access: bool,
    pub tiers: tiers::Policy,
    /// The window is at a sign-in or payment step: by its title, or by what the last snapshot of it showed.
    pub step: Option<&'a critical::Critical>,
    /// Another session has the desktop: what the lease says to this one (`Desk::check_at`).
    pub busy: Option<&'a str>,
}

/// The settings a classification reads: this process's, or fixed ones (a test, a second desktop).
#[derive(Debug, Clone, Copy)]
enum Settings {
    Process,
    Fixed { full_access: bool, tiers: tiers::Policy },
}

impl Settings {
    fn full_access(self) -> bool {
        match self {
            Settings::Process => crate::full_access::is_on(),
            Settings::Fixed { full_access, .. } => full_access,
        }
    }

    fn tiers(self) -> tiers::Policy {
        match self {
            Settings::Process => tiers::Policy::from_env(),
            Settings::Fixed { tiers, .. } => tiers,
        }
    }
}

type WindowReader = Arc<dyn Fn() -> Option<Foreground> + Send + Sync>;

/// What a classification saw, kept until the call it was for is made: the window in front, who asked, and whether it was a question already. The call looks at the window again
/// (card R6-08, T6-08.7).
#[derive(Debug, Clone)]
struct Seen {
    key: String,
    front: Option<Foreground>,
    caller: GuardCaller,
    /// The classification was a question (the guard asks about this call): a question at the call is then the one that was put.
    asked: bool,
}

/// How many classifications wait for their call: a call that is refused (by the person, by a hook) never comes, and its entry goes when newer ones push it out.
const MAX_SEEN: usize = 64;

/// Who the guard calls this caller: the voice is the voice, the app's own direct calls and the API are the API, anyone else is the agent.
pub fn guard_caller_for(caller: Caller) -> GuardCaller {
    match caller {
        Caller::Voice => GuardCaller::Voice,
        Caller::Api | Caller::Ui => GuardCaller::Api,
        Caller::Agent | Caller::McpClient | Caller::Replay => GuardCaller::Agent,
    }
}

/// The session that a call belongs to, for the one-session-at-a-time lease: the run, else the one named on this thread, else the kind of caller.
fn owner_of(run: &str, named_here: Option<String>, caller: GuardCaller) -> String {
    if !run.is_empty() {
        return run.to_string();
    }
    named_here.unwrap_or_else(|| caller_backend(caller).to_string())
}

/// The key a classification is kept under until its call is made: the run, the tool and the arguments (the host passes the same validated arguments to both).
fn seen_key(run: &str, tool: &str, args: &Value) -> String {
    format!("{run}|{tool}|{}", aqua_tool_host::short_digest(tool, args))
}

fn refusal(kind: DenyKind, why: String) -> ToolError {
    match kind {
        DenyKind::Blocked => ToolError::new(ErrorCode::PermissionDenied, why),
        DenyKind::Invalid => ToolError::new(ErrorCode::InvalidArguments, why),
    }
}

/// Everything that comes between the question and the server (card R6-08, T6-08.7): a call that acts on the screen looks at the window in front again and is refused if it is
/// another program or another window than the one the classification saw (a person who took a minute to say yes said it for a window), if it is one that is refused outright (Aqua's
/// own), or if the same window is at a sign-in or payment step now and was not when the call was judged (a page that went on to the payment while the person was deciding);
/// it then takes the desktop for its session, or is refused with the name of the session that has it. A call that only looks keeps its own lease alive.
#[allow(clippy::too_many_arguments)]
fn prepare_call(
    desk: &session::Desk,
    wire: &str,
    args: &Value,
    run: &str,
    named_here: Option<String>,
    seen: Option<&Seen>,
    front_now: Option<&Foreground>,
    step_now: Option<&critical::Critical>,
    settings: Settings,
    now: Instant,
) -> Result<(), ToolError> {
    let caller = seen.map(|s| s.caller).unwrap_or(GuardCaller::Agent);
    let owner = owner_of(run, named_here, caller);
    if is_observe_call(wire, args) {
        desk.touch_at(&owner, now);
        return Ok(());
    }
    if let Some(s) = seen {
        if input_target_changed(s.front.as_ref(), front_now) {
            return Err(ToolError::new(
                ErrorCode::Conflict,
                format!("the window in front changed before this was done ({} is now in front): nothing was done. Look at the screen again and ask again if you still want it.", describe(front_now)),
            ));
        }
    }
    // The window in front now is judged as any other: it may be one that is refused outright, or one that asks a question that was not asked when the call was judged.
    let mut verdict = evaluate_tiered(wire, args, caller, front_now, settings.full_access(), settings.tiers());
    if front_now.is_some() {
        verdict = critical::critical_verdict(verdict, wire, args, caller, front_now, step_now);
    }
    match verdict {
        Verdict::Deny(kind, why) => return Err(refusal(kind, why)),
        Verdict::Ask(why) if seen.is_some_and(|s| !s.asked) => {
            return Err(ToolError::new(
                ErrorCode::Conflict,
                format!("the window in front needs a question now that was not put when this was judged ({why}): nothing was done. Look at the screen again and ask again if you still want it."),
            ));
        }
        Verdict::Allow | Verdict::Ask(_) => {}
    }
    desk.claim_at(&owner, now).map_err(|why| ToolError::new(ErrorCode::PermissionDenied, why))
}

pub struct WindowsUiProvider {
    client: Arc<Mutex<StdioMcpClient>>,
    /// The lease on the desktop and what the last snapshot showed: the process's own (the one the guard and the Esc key use), or a test's.
    screen: Screen,
    window: WindowReader,
    settings: Settings,
    seen: Mutex<VecDeque<Seen>>,
}

impl WindowsUiProvider {
    pub fn new(client: Arc<Mutex<StdioMcpClient>>) -> Self {
        Self { client, screen: Screen::global(), window: Arc::new(foreground_window), settings: Settings::Process, seen: Mutex::new(VecDeque::new()) }
    }

    /// A provider with a desktop of its own (its lease and its view of the last snapshot are not the process's), the window in front given, and the full-access switch and the
    /// tier setting given: for a test, which must not depend on the screen it runs on or on another test's settings.
    pub fn isolated(client: Arc<Mutex<StdioMcpClient>>, window: Arc<dyn Fn() -> Option<Foreground> + Send + Sync>, full_access: bool, tiers: tiers::Policy) -> Self {
        Self { client, screen: Screen::own(), window, settings: Settings::Fixed { full_access, tiers }, seen: Mutex::new(VecDeque::new()) }
    }

    fn wire_name(host_name: &str) -> Option<&'static str> {
        specs().into_iter().find(|s| s.0 == host_name).map(|s| s.1)
    }

    /// The classes of a question the guard puts: a change, and what could go wrong with it. A shell or an administration console is a privileged one; the rest could send
    /// something to somebody (Enter in a chat, a step where money or a password is asked for).
    pub fn question_classes(why: &str) -> Vec<PermClass> {
        vec![PermClass::Write, if shell_or_admin_reason(why) { PermClass::Privileged } else { PermClass::ExternalEffect }]
    }

    /// What the guard's rules say about a call, as `caller`, for the desktop **given** (card R6-08), so that the one-door table can ask it without a desktop.
    /// [`ToolProvider::classify_for`] is this with the window in front now, this process's settings, the sign-in or payment step the last snapshot showed, and the lease.
    ///
    /// The order is the guard's (`WindowsGuard::call_at`): its rules and the tier of the window, then a sign-in or payment step (asked about whatever the settings are, refused to the
    /// voice), then one session at a time. A refusal of the rules comes before the lease, as it does there.
    pub fn classify_with(tool: &str, args: &Value, caller: GuardCaller, desktop: &Desktop<'_>) -> Result<Classification, ToolError> {
        let wire = Self::wire_name(tool).ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("unknown tool '{tool}'")))?;
        let mut verdict = evaluate_tiered(wire, args, caller, desktop.front, desktop.full_access, desktop.tiers);
        // A step is looked for only where the window in front is known, as the guard does.
        if desktop.front.is_some() {
            verdict = critical::critical_verdict(verdict, wire, args, caller, desktop.front, desktop.step);
        }
        if !matches!(verdict, Verdict::Deny(..)) && !is_observe_call(wire, args) {
            if let Some(why) = desktop.busy {
                return Err(ToolError::new(ErrorCode::PermissionDenied, why));
            }
        }
        match verdict {
            Verdict::Deny(kind, why) => Err(refusal(kind, why)),
            Verdict::Ask(why) => Ok(Classification { classes: Self::question_classes(&why), target: wire.to_string(), summary: why, target_allowed: false }),
            Verdict::Allow => Ok(Classification {
                classes: if is_observe_call(wire, args) { vec![PermClass::Read] } else { vec![PermClass::Write] },
                target: wire.to_string(),
                summary: format!("{wire} on the desktop"),
                target_allowed: true,
            }),
        }
    }

    /// Does this call send what it holds into another program (card R6-08)? Typing with Enter, in the flag or in the text, and a shortcut with Enter in it.
    pub fn call_sends_something_out(tool: &str, args: &Value) -> bool {
        match Self::wire_name(tool) {
            Some("Type") => presses_enter(args),
            Some("Shortcut") => {
                let combo = args.get("shortcut").and_then(|v| v.as_str()).unwrap_or("").to_ascii_lowercase();
                combo.split('+').map(str::trim).any(|k| matches!(k, "enter" | "return" | "numpadenter"))
            }
            _ => false,
        }
    }

    /// The desktop as it is now, for `caller` and `run`, judged: the classification, the window it was judged for, and who the guard takes the caller for.
    fn judge(&self, tool: &str, args: &Value, caller: Caller, run: &str) -> Result<(Classification, Option<Foreground>, GuardCaller), ToolError> {
        let guard_caller = guard_caller_for(caller);
        let front = (self.window)();
        let step = front.as_ref().and_then(|f| self.screen.step_in(f));
        let owner = owner_of(run, session::current_owner(), guard_caller);
        let busy = self.screen.desk().check_at(&owner, Instant::now()).err();
        let desktop = Desktop { front: front.as_ref(), full_access: self.settings.full_access(), tiers: self.settings.tiers(), step: step.as_ref(), busy: busy.as_deref() };
        let class = Self::classify_with(tool, args, guard_caller, &desktop)?;
        Ok((class, front, guard_caller))
    }

    fn remember(&self, seen: Seen) {
        let mut kept = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        kept.retain(|s| s.key != seen.key);
        kept.push_back(seen);
        while kept.len() > MAX_SEEN {
            kept.pop_front();
        }
    }

    fn take_seen(&self, key: &str) -> Option<Seen> {
        let mut kept = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let at = kept.iter().position(|s| s.key == key)?;
        kept.remove(at)
    }
}

impl ToolProvider for WindowsUiProvider {
    fn id(&self) -> &str {
        "windows-ui"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        specs()
            .into_iter()
            .map(|(name, wire, description, props, extra)| {
                let observe = is_observe_tool(wire);
                let mut schema = json!({"type": "object", "properties": props});
                if extra {
                    schema["additionalProperties"] = json!(true);
                }
                if wire == "Type" {
                    schema["required"] = json!(["text"]);
                }
                if wire == "Shortcut" {
                    schema["required"] = json!(["shortcut"]);
                }
                ToolManifest {
                    name: name.to_string(),
                    title: wire.to_string(),
                    description: description.to_string(),
                    classes: if observe { vec![PermClass::Read] } else { vec![PermClass::Write] },
                    default_decision: Decision::Auto,
                    input_schema: schema,
                    timeout_default_ms: 30_000,
                    timeout_max_ms: 120_000,
                    annotations: Annotations { read_only: observe, destructive: false, idempotent: observe, open_world: true },
                    // What is on screen (a web page, a document, a chat) is content from outside.
                    untrusted_output: true,
                }
            })
            .collect()
    }

    /// The desktop is part of the one door (card R6-08): with the switch on, the host decides these calls by the door's rule.
    fn door_applies(&self) -> bool {
        true
    }

    /// A skill's list may name a desktop tool as the model writes it (`Click`) as well as the host's name (`win.click`).
    fn restriction_names(&self, tool: &str) -> Vec<String> {
        Self::wire_name(tool).map(|w| vec![w.to_string()]).unwrap_or_default()
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        self.judge(tool, args, Caller::Agent, "").map(|(class, _, _)| class)
    }

    fn classify_for(&self, tool: &str, args: &Value, ctx: &ClassifyContext) -> Result<Classification, ToolError> {
        let (class, front, caller) = self.judge(tool, args, ctx.caller, &ctx.run)?;
        self.remember(Seen { key: seen_key(&ctx.run, tool, args), front, caller, asked: !class.target_allowed });
        Ok(class)
    }

    fn sends_something_out(&self, tool: &str, args: &Value) -> bool {
        Self::call_sends_something_out(tool, args)
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let Some(wire) = Self::wire_name(tool) else {
            let missing = ToolError::new(ErrorCode::NotFound, format!("unknown tool '{tool}'"));
            return Box::pin(async move { Err::<ProviderOutput, ToolError>(missing) });
        };
        // Everything that must happen between the question and the server happens here, on the thread of the run, before the work is handed to another one.
        let seen = self.take_seen(&seen_key(&ctx.agent_run_id, tool, &args));
        let front = (self.window)();
        let step_now = front.as_ref().and_then(|f| self.screen.step_in(f));
        let prepared = prepare_call(self.screen.desk(), wire, &args, &ctx.agent_run_id, session::current_owner(), seen.as_ref(), front.as_ref(), step_now.as_ref(), self.settings, Instant::now());
        let client = self.client.clone();
        Box::pin(async move {
            prepared?;
            let looked_at = matches!(wire, "Snapshot" | "WaitFor").then_some(front).flatten();
            let text = tokio::task::spawn_blocking(move || forward(&client, wire, &args))
                .await
                .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("desktop task failed: {e}")))?
                .map_err(|e| match e {
                    ForwardError::Unavailable(m) => ToolError::new(ErrorCode::Unavailable, format!("Windows-MCP is not running: {m}")),
                    ForwardError::Backend(m) => ToolError::new(ErrorCode::BackendError, m),
                })?;
            // What the screen said is what a later call at this window is judged by (a sign-in or payment step the title does not give away).
            if let Some(f) = looked_at.as_ref() {
                self.screen.note_snapshot(f, &text);
            }
            Ok(ProviderOutput { data: json!({ "text": text.clone() }), text, private: true, ..Default::default() })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::McpTool;
    use std::sync::atomic::AtomicBool;

    fn client() -> Arc<Mutex<StdioMcpClient>> {
        Arc::new(Mutex::new(StdioMcpClient::new(crate::windows_mcp::default_windows_config())))
    }

    fn window(exe: &str, title: &str) -> Foreground {
        Foreground { exe: exe.into(), title: title.into(), window: 1 }
    }

    /// A provider with a desktop of its own and `front` in front, under the strict tiers and without full access: nothing here depends on the screen the test runs on, on the
    /// process's full-access switch, or on another test's lease.
    fn provider_at(front: Option<Foreground>) -> WindowsUiProvider {
        WindowsUiProvider::isolated(client(), Arc::new(move || front.clone()), false, tiers::Policy::Strict)
    }

    fn provider() -> WindowsUiProvider {
        provider_at(Some(window("notepad.exe", "Untitled - Notepad")))
    }

    fn desktop(front: Option<&Foreground>, full_access: bool) -> Desktop<'_> {
        Desktop { front, full_access, tiers: tiers::Policy::Strict, step: None, busy: None }
    }

    fn asked_as(caller: Caller, run: &str) -> ClassifyContext {
        ClassifyContext { caller, run: run.into(), tainted: false, private: false }
    }

    #[test]
    fn all_fourteen_ui_tools_are_provided_with_valid_manifests() {
        let m = provider().manifests();
        assert_eq!(m.len(), 14);
        for t in &m {
            t.check().unwrap_or_else(|e| panic!("{e}"));
            assert!(t.name.starts_with("win."));
        }
        let mut got: Vec<&str> = specs().iter().map(|s| s.1).collect();
        let mut want: Vec<&str> = crate::windows_mcp::guard::WINDOWS_UI_TOOLS.to_vec();
        got.sort();
        want.sort();
        assert_eq!(got, want, "provider must cover exactly the guard allowlist");
    }

    #[test]
    fn app_schema_has_no_executable_args_or_cwd() {
        let app = provider().manifests().into_iter().find(|m| m.name == "win.app").unwrap();
        let props = app.input_schema["properties"].as_object().unwrap();
        for banned in ["executable", "args", "cwd"] {
            assert!(!props.contains_key(banned));
        }
        let err = aqua_tool_host::validate_args(&app.input_schema, &json!({"mode": "launch", "executable": "powershell.exe"})).unwrap_err();
        assert!(err.join(" ").contains("unknown argument 'executable'"));
    }

    /// What the pinned server lists, as captured in `core/tests/fixtures/windows_mcp/tools_list.json`.
    fn upstream() -> Vec<Value> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../core/tests/fixtures/windows_mcp/tools_list.json");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        serde_json::from_str::<Value>(&text).unwrap()["tools"].as_array().cloned().unwrap()
    }

    #[test]
    fn the_fixture_is_exactly_the_tools_the_server_is_started_with_and_aqua_uses_only_what_the_server_has() {
        let up = upstream();
        let mut fixture: Vec<&str> = up.iter().map(|t| t["name"].as_str().unwrap()).collect();
        let mut launched: Vec<&str> = crate::windows_mcp::guard::WINDOWS_UI_TOOLS.to_vec();
        fixture.sort();
        launched.sort();
        assert_eq!(fixture, launched, "the fixture lists the tools Aqua starts the server with, no more and no fewer");
        // Every argument Aqua offers for a tool is one the server takes: a schema that drifts from the server's fails here, not at a call.
        for (host_name, wire, _description, props, _extra) in specs() {
            let theirs = up.iter().find(|t| t["name"] == wire).unwrap_or_else(|| panic!("{wire} is not in the fixture"));
            let known = theirs["inputSchema"]["properties"].as_object().cloned().unwrap_or_default();
            for arg in props.as_object().unwrap().keys() {
                assert!(known.contains_key(arg), "{host_name}: Aqua offers '{arg}', which {wire} does not take (it takes: {:?})", known.keys().collect::<Vec<_>>());
            }
        }
        // The two arguments the drag rule relies on are really the server's.
        let mv = up.iter().find(|t| t["name"] == "Move").unwrap();
        assert!(mv["inputSchema"]["properties"].get("drag").is_some() && mv["inputSchema"]["properties"].get("from_loc").is_some());
    }

    /// Starts the pinned server (offline, from the uv cache, as Aqua does) and compares what it lists with the fixture, by the same hash
    /// the tool lock uses. Run on purpose, after an upgrade or when the lock says "changed": `cargo test -p aqua-mcp-adapters live_ -- --ignored`.
    #[test]
    #[ignore = "starts the pinned Windows-MCP from the uv cache; run after an upgrade (see documentation/MCP_UPGRADES.md)"]
    fn live_windows_mcp_lists_what_the_fixture_says() {
        let mut client = StdioMcpClient::new(crate::windows_mcp::default_windows_config());
        if let Err(e) = client.start() {
            eprintln!("Windows-MCP is not installed here (the pinned version is not in the uv cache): {e:#}. Nothing to compare.");
            return;
        }
        let live = client.cached_tools().to_vec();
        let _ = client.shutdown();
        let fixture: Vec<McpTool> = upstream()
            .into_iter()
            .map(|t| McpTool { name: t["name"].as_str().unwrap().into(), description: t["description"].as_str().unwrap_or("").into(), input_schema: t["inputSchema"].clone(), source: None })
            .collect();
        let hashes = |tools: &[McpTool]| tools.iter().map(|t| (t.name.clone(), crate::tool_lock::fingerprint(t))).collect::<std::collections::BTreeMap<_, _>>();
        let (now, then) = (hashes(&live), hashes(&fixture));
        let differing: Vec<&String> = now.iter().filter(|(n, h)| then.get(*n) != Some(*h)).map(|(n, _)| n).chain(then.keys().filter(|n| !now.contains_key(*n))).collect();
        assert!(differing.is_empty(), "the server's tools differ from the fixture: {differing:?}. If the upgrade is intended, refresh core/tests/fixtures/windows_mcp/tools_list.json and read what changed first.");
    }

    /// Card R6-08: the classification is the guard's rules for the window it is given, with nothing read from the desktop, so the one-door table can ask it for any window.
    #[test]
    fn the_classification_is_the_guards_answer_for_the_window_and_the_settings_it_is_given() {
        use PermClass::*;
        let notepad = window("notepad.exe", "Untitled - Notepad");
        let classify = |tool: &str, args: Value, fg: Option<&Foreground>, full: bool| WindowsUiProvider::classify_with(tool, &args, GuardCaller::Agent, &desktop(fg, full));
        let click = json!({"loc": [10, 10]});
        let c = classify("win.click", click.clone(), Some(&notepad), false).unwrap();
        assert_eq!((c.classes, c.target_allowed), (vec![Write], true), "a click in a plain window is a change the provider vouches for");
        let c = classify("win.snapshot", json!({}), Some(&notepad), false).unwrap();
        assert_eq!((c.classes, c.target_allowed), (vec![Read], true));
        // Typing with Enter in one call is a send: a question without full access, the same as any typing with it.
        for args in [json!({"text": "hi", "press_enter": true}), json!({"text": "hi\n"})] {
            let c = classify("win.type", args.clone(), Some(&notepad), false).unwrap();
            assert_eq!((c.classes, c.target_allowed), (vec![Write, ExternalEffect], false), "{args}");
            assert!(c.summary.contains("press Enter"), "{}", c.summary);
            assert!(classify("win.type", args, Some(&notepad), true).unwrap().target_allowed, "full access lets it through, as the guard does");
        }
        // A browser is view-only for the desktop tools; Aqua's own window and the shells are refused; the relaxed setting brings the old questions back.
        let chrome = window("chrome.exe", "Docs - Google Chrome");
        assert_eq!(classify("win.click", click.clone(), Some(&chrome), false).unwrap_err().code, ErrorCode::PermissionDenied);
        assert!(classify("win.click", click.clone(), Some(&chrome), true).is_err(), "full access does not widen a tier");
        let relaxed = Desktop { tiers: tiers::Policy::Relaxed, ..desktop(Some(&chrome), false) };
        assert!(WindowsUiProvider::classify_with("win.click", &click, GuardCaller::Agent, &relaxed).is_ok());
        assert_eq!(classify("win.click", click.clone(), Some(&window("aqua-desktop.exe", "Aqua Assistant")), true).unwrap_err().code, ErrorCode::PermissionDenied);
        assert_eq!(classify("win.shortcut", json!({"shortcut": "win+r"}), Some(&notepad), true).unwrap_err().code, ErrorCode::PermissionDenied);
        assert_eq!(classify("win.app", json!({"mode": "launch", "name": "PowerShell"}), None, true).unwrap_err().code, ErrorCode::PermissionDenied);
        // Not knowing the window in front is a question, unless full access is on.
        assert_eq!(classify("win.click", click.clone(), None, false).unwrap().classes, vec![Write, ExternalEffect]);
        assert!(classify("win.click", click, None, true).unwrap().target_allowed);
        assert_eq!(classify("win.nothing", json!({}), None, false).unwrap_err().code, ErrorCode::NotFound);
    }

    #[test]
    fn a_call_that_sends_into_another_program_is_known_by_the_provider_and_one_that_only_types_is_not() {
        let sends = |tool: &str, args: Value| WindowsUiProvider::call_sends_something_out(tool, &args);
        assert!(sends("win.type", json!({"text": "hi", "press_enter": true})));
        assert!(sends("win.type", json!({"text": "hi\r\n"})));
        assert!(sends("win.type", json!({"text": "hi", "press_enter": "true"})));
        assert!(!sends("win.type", json!({"text": "hi"})) && !sends("win.type", json!({"text": "hi", "press_enter": false})));
        for combo in ["ctrl+enter", "Control + Enter", "enter", "ctrl+return"] {
            assert!(sends("win.shortcut", json!({"shortcut": combo})), "{combo}");
        }
        assert!(!sends("win.shortcut", json!({"shortcut": "ctrl+s"})) && !sends("win.click", json!({"loc": [1, 1]})) && !sends("win.snapshot", json!({})));
        assert!(!sends("win.unknown", json!({"press_enter": true})));
        // And the provider says so through the host's trait.
        assert!(provider().sends_something_out("win.type", &json!({"text": "x", "press_enter": true})));
    }

    /// Card R6-08 (three of the five rows the golden table showed the host looser on): a sign-in or payment step is a question through the provider whatever the settings are, as it is
    /// through the guard, and never the voice's. It was red while only `WindowsGuard::call_at` knew the step.
    #[test]
    fn a_sign_in_or_payment_step_is_a_question_through_the_provider_as_it_is_through_the_guard_and_never_the_voices() {
        use PermClass::*;
        let bank = window("bank.exe", "Sign in to your account");
        let shop = window("shop.exe", "Checkout - Shop");
        let sign_in = critical::critical_point(&bank.title, "").expect("the title says sign in");
        let payment = critical::critical_point(&shop.title, "").expect("the title says checkout");
        let at = |tool: &str, args: Value, caller: GuardCaller, front: &Foreground, step: Option<&critical::Critical>, full: bool| {
            WindowsUiProvider::classify_with(tool, &args, caller, &Desktop { front: Some(front), full_access: full, tiers: tiers::Policy::Strict, step, busy: None })
        };
        let card = "4111-1111-1111-1111";
        for full in [false, true] {
            let c = at("win.click", json!({"loc": [10, 10]}), GuardCaller::Agent, &bank, Some(&sign_in), full).unwrap();
            assert_eq!((c.classes.clone(), c.target_allowed), (vec![Write, ExternalEffect], false), "full access {full}: {c:?}");
            assert!(c.summary.contains("sign-in step"), "{}", c.summary);
            let c = at("win.type", json!({"text": card}), GuardCaller::Api, &shop, Some(&payment), full).unwrap();
            assert_eq!((c.classes.clone(), c.target_allowed), (vec![Write, ExternalEffect], false), "full access {full}: {c:?}");
            assert!(c.summary.contains("payment step") && !c.summary.contains("4111"), "what was typed is not in the question: {}", c.summary);
        }
        // Looking is not a step's business.
        let c = at("win.snapshot", json!({}), GuardCaller::Agent, &bank, Some(&sign_in), false).unwrap();
        assert_eq!((c.classes, c.target_allowed), (vec![Read], true));
        // By voice it is refused outright, and what would have been typed is not in the answer.
        let e = at("win.type", json!({"text": card}), GuardCaller::Voice, &bank, Some(&sign_in), false).unwrap_err();
        assert!(e.code == ErrorCode::PermissionDenied && e.message.contains("never by voice") && !e.message.contains("4111"), "{e:?}");
        assert_eq!(at("win.click", json!({"loc": [1, 1]}), GuardCaller::Voice, &bank, Some(&sign_in), true).unwrap_err().code, ErrorCode::PermissionDenied);
        // The provider is told about a step: with none given, the same click in the same window is the plain one (the table's five rows were this).
        assert!(at("win.click", json!({"loc": [10, 10]}), GuardCaller::Agent, &bank, None, true).unwrap().target_allowed);
        // A step is looked for only where the window in front is known, as the guard does.
        let unknown = Desktop { front: None, full_access: true, tiers: tiers::Policy::Strict, step: Some(&sign_in), busy: None };
        assert!(WindowsUiProvider::classify_with("win.click", &json!({"loc": [10, 10]}), GuardCaller::Agent, &unknown).unwrap().target_allowed);
    }

    /// Card R6-08: one session at a time, through the provider. A call that acts while another session has the desktop is refused before anyone is asked, in the lease's words; a look
    /// is not; a refusal of the rules comes first, in the rules' words.
    #[test]
    fn a_call_that_acts_on_the_screen_while_another_session_has_it_is_refused_before_anyone_is_asked_and_a_look_is_not() {
        let notepad = window("notepad.exe", "Untitled - Notepad");
        let busy = "another session (run_a) is using the desktop: wait for it to finish, or stop it, before this one acts on the screen";
        let with = |tool: &str, args: Value, front: Option<&Foreground>| {
            WindowsUiProvider::classify_with(tool, &args, GuardCaller::Agent, &Desktop { front, full_access: false, tiers: tiers::Policy::Strict, step: None, busy: Some(busy) })
        };
        let e = with("win.click", json!({"loc": [1, 1]}), Some(&notepad)).unwrap_err();
        assert_eq!((e.code, e.message.as_str()), (ErrorCode::PermissionDenied, busy));
        assert!(with("win.snapshot", json!({}), Some(&notepad)).is_ok());
        assert!(with("win.move", json!({"loc": [1, 1]}), Some(&notepad)).is_ok(), "moving the pointer only looks");
        assert!(with("win.move", json!({"loc": [1, 1], "drag": true}), Some(&notepad)).is_err(), "a drag acts");
        // A question is refused too, before it is asked: Enter in a chat window.
        let chat = window("slack.exe", "Slack - team");
        assert_eq!(with("win.type", json!({"text": "hi", "press_enter": true}), Some(&chat)).unwrap_err().message, busy);
        // A refusal of the rules is the guard's, in its words, and not the lease's.
        let aqua = window("aqua-desktop.exe", "Aqua Assistant");
        let e = with("win.click", json!({"loc": [1, 1]}), Some(&aqua)).unwrap_err();
        assert!(e.message.contains("approval prompts") && !e.message.contains("another session"), "{e:?}");
    }

    /// Card R6-08, decisions D-13 and D-14: the voice is judged as the voice by the provider. It types text into the window in front and opens an app by name, and does nothing
    /// else that acts; where the agent would be asked, it is refused.
    #[test]
    fn the_voice_is_judged_as_the_voice_it_types_text_into_the_window_in_front_and_opens_an_app_by_name_and_nothing_else_that_acts() {
        let notepad = window("notepad.exe", "Untitled - Notepad");
        let voice = |tool: &str, args: Value, front: Option<&Foreground>| WindowsUiProvider::classify_with(tool, &args, GuardCaller::Voice, &desktop(front, false));
        let c = voice("win.type", json!({"text": "hello"}), Some(&notepad)).unwrap();
        assert_eq!((c.classes, c.target_allowed), (vec![PermClass::Write], true));
        let c = voice("win.app", json!({"mode": "launch", "name": "Notepad"}), Some(&notepad)).unwrap();
        assert_eq!((c.classes, c.target_allowed), (vec![PermClass::Write], true));
        assert!(voice("win.app", json!({"mode": "switch", "name": "Notepad"}), None).is_ok());
        let c = voice("win.snapshot", json!({}), None).unwrap();
        assert_eq!((c.classes, c.target_allowed), (vec![PermClass::Read], true));
        for (tool, args) in [
            ("win.type", json!({"text": "hello", "press_enter": true})),
            ("win.type", json!({"text": "hello\n"})),
            ("win.type", json!({"text": "hello", "loc": [1, 1]})),
            ("win.type", json!({"text": "  "})),
            ("win.click", json!({"loc": [1, 1]})),
            ("win.shortcut", json!({"shortcut": "ctrl+s"})),
            ("win.app", json!({"mode": "resize", "name": "Notepad"})),
            ("win.app", json!({"mode": "launch", "name": "PowerShell"})),
            ("win.multi_edit", json!({"locs": [[1, 1]], "text": "x"})),
            ("win.move", json!({"loc": [1, 1], "drag": true})),
        ] {
            let e = voice(tool, args.clone(), Some(&notepad)).unwrap_err();
            assert_eq!(e.code, ErrorCode::PermissionDenied, "{tool} {args}: {e:?}");
        }
        // Where the agent would be asked, the voice is refused: a browser in front, a window nobody could name, a shell, Aqua's own window.
        for front in [Some(window("chrome.exe", "Facebook")), None, Some(window("powershell.exe", "Windows PowerShell")), Some(window("aqua-desktop.exe", "Aqua Assistant"))] {
            let e = voice("win.type", json!({"text": "hello"}), front.as_ref()).unwrap_err();
            assert_eq!(e.code, ErrorCode::PermissionDenied, "{front:?}: {e:?}");
        }
    }

    #[test]
    fn who_calls_is_who_the_guard_takes_them_for_and_the_session_is_the_run_else_the_name_on_the_thread_else_the_kind_of_caller() {
        for (caller, want) in [
            (Caller::Voice, GuardCaller::Voice),
            (Caller::Api, GuardCaller::Api),
            (Caller::Ui, GuardCaller::Api),
            (Caller::Agent, GuardCaller::Agent),
            (Caller::McpClient, GuardCaller::Agent),
            (Caller::Replay, GuardCaller::Agent),
        ] {
            assert_eq!(guard_caller_for(caller), want, "{caller:?}");
        }
        assert_eq!(owner_of("run_a", Some("named".into()), GuardCaller::Voice), "run_a");
        assert_eq!(owner_of("", Some("named".into()), GuardCaller::Voice), "named");
        assert_eq!(owner_of("", None, GuardCaller::Voice), "windows:voice");
        assert_eq!(owner_of("", None, GuardCaller::Api), "windows:api");
        assert_eq!(owner_of("", None, GuardCaller::Agent), "windows:agent");
    }

    /// The classification through the trait reads what the provider itself holds: the window in front, what the last snapshot of it showed, and the lease.
    #[test]
    fn the_provider_reads_the_window_the_last_snapshot_and_the_lease_it_has_when_the_host_asks_it() {
        let page = Foreground { exe: "contoso.exe".into(), title: "Contoso".into(), window: 9 };
        let p = provider_at(Some(page.clone()));
        let click = json!({"loc": [10, 10]});
        // Nothing is known of the page: a click in a plain program is allowed.
        assert!(p.classify_for("win.click", &click, &asked_as(Caller::Agent, "run_a")).unwrap().target_allowed);
        // A snapshot of it was read (by the call that made it), and a payment step is found from what it said although the title gives nothing away.
        p.screen.note_snapshot(&page, "Card number CVV Pay now");
        let c = p.classify_for("win.click", &click, &asked_as(Caller::Agent, "run_a")).unwrap();
        assert!(!c.target_allowed && c.classes.contains(&PermClass::ExternalEffect) && c.summary.contains("payment step"), "{c:?}");
        // The voice is refused there, and a look is not asked about.
        let e = p.classify_for("win.type", &json!({"text": "hi"}), &asked_as(Caller::Voice, "said")).unwrap_err();
        assert!(e.message.contains("never by voice"), "{e:?}");
        let c = p.classify_for("win.snapshot", &json!({}), &asked_as(Caller::Agent, "run_a")).unwrap();
        assert_eq!((c.classes, c.target_allowed), (vec![PermClass::Read], true));
        // Another session has the desktop: an acting call of this run is refused, in the lease's words, even where it would have been a question; the owner keeps it.
        p.screen.desk().claim_at("run_x", Instant::now()).unwrap();
        let e = p.classify_for("win.click", &click, &asked_as(Caller::Agent, "run_a")).unwrap_err();
        assert!(e.code == ErrorCode::PermissionDenied && e.message.contains("another session (run_x)"), "{e:?}");
        assert!(p.classify_for("win.snapshot", &json!({}), &asked_as(Caller::Agent, "run_a")).is_ok(), "looking is not held");
        assert!(p.classify_for("win.click", &click, &asked_as(Caller::Agent, "run_x")).is_ok(), "the session that has it keeps it");
    }

    /// The window a classification saw is kept for the call it was for (card R6-08, T6-08.7): once, replaced by a newer one of the same call, and bounded.
    #[test]
    fn a_classification_is_kept_for_its_call_once_replaced_by_a_newer_one_of_the_same_call_and_bounded() {
        let p = provider();
        let click = json!({"loc": [10, 10]});
        let ctx = asked_as(Caller::Agent, "run_a");
        p.classify_for("win.click", &click, &ctx).unwrap();
        p.classify_for("win.click", &click, &ctx).unwrap();
        assert_eq!(p.seen.lock().unwrap().len(), 1, "the same call twice is one entry");
        let key = seen_key("run_a", "win.click", &click);
        let seen = p.take_seen(&key).expect("kept");
        assert_eq!((seen.front.map(|f| f.exe), seen.caller), (Some("notepad.exe".to_string()), GuardCaller::Agent));
        assert!(p.take_seen(&key).is_none(), "a classification is for one call");
        assert!(p.take_seen(&seen_key("run_b", "win.click", &click)).is_none(), "another run's call is another entry");
        for i in 0..(MAX_SEEN + 10) {
            p.classify_for("win.click", &json!({"loc": [i, 0]}), &ctx).unwrap();
        }
        assert_eq!(p.seen.lock().unwrap().len(), MAX_SEEN, "calls that were refused never come, and are pushed out by newer ones");
        assert!(p.take_seen(&seen_key("run_a", "win.click", &json!({"loc": [0, 0]}))).is_none(), "the oldest went first");
        assert!(p.take_seen(&seen_key("run_a", "win.click", &json!({"loc": [MAX_SEEN + 9, 0]}))).is_some());
        // A call that was refused when it was judged is not kept at all.
        let before = p.seen.lock().unwrap().len();
        assert!(p.classify_for("win.click", &json!({"loc": [1, 1], "button": "right"}), &asked_as(Caller::Voice, "said")).is_err());
        assert_eq!(p.seen.lock().unwrap().len(), before);
    }

    /// Card R6-08, T6-08.7: everything that comes between the question and the server. The window in front is read again, and a call that acts is refused if it changed.
    #[test]
    fn a_call_looks_at_the_window_again_and_takes_the_desktop_only_if_nothing_changed_and_nobody_else_has_it() {
        let notepad = Foreground { exe: "notepad.exe".into(), title: "Untitled".into(), window: 1 };
        let mail = Foreground { exe: "outlook.exe".into(), title: "Inbox".into(), window: 2 };
        let seen_at = |front: &Foreground, caller: GuardCaller| Seen { key: "k".into(), front: Some(front.clone()), caller, asked: false };
        let settings = Settings::Fixed { full_access: false, tiers: tiers::Policy::Strict };
        let now = Instant::now();
        let click = json!({"loc": [1, 1]});
        let go = |desk: &session::Desk, run: &str, wire: &str, args: &Value, seen: Option<&Seen>, front: Option<&Foreground>| prepare_call(desk, wire, args, run, None, seen, front, None, settings, now);
        // Nothing changed: the call takes the desktop for its run.
        let desk = session::Desk::new();
        assert!(go(&desk, "run_a", "Click", &click, Some(&seen_at(&notepad, GuardCaller::Agent)), Some(&notepad)).is_ok());
        assert_eq!(desk.holder_at(now).as_deref(), Some("run_a"));
        // Another program came to the front while the person was asked: nothing is sent, and the desktop is not taken.
        let desk = session::Desk::new();
        let e = go(&desk, "run_a", "Click", &click, Some(&seen_at(&notepad, GuardCaller::Agent)), Some(&mail)).unwrap_err();
        assert!(e.code == ErrorCode::Conflict && e.message.contains("changed") && e.message.contains("outlook.exe"), "{e:?}");
        assert_eq!(desk.holder_at(now), None);
        // Another window of the same program, and a window that can no longer be read, are changes too; the same window under a new title (text was typed) is not.
        let other_window = Foreground { window: 5, ..notepad.clone() };
        assert_eq!(go(&desk, "run_a", "Click", &click, Some(&seen_at(&notepad, GuardCaller::Agent)), Some(&other_window)).unwrap_err().code, ErrorCode::Conflict);
        assert_eq!(go(&desk, "run_a", "Click", &click, Some(&seen_at(&notepad, GuardCaller::Agent)), None).unwrap_err().code, ErrorCode::Conflict);
        let retitled = Foreground { title: "*Untitled".into(), ..notepad.clone() };
        assert!(go(&desk, "run_a", "Click", &click, Some(&seen_at(&notepad, GuardCaller::Agent)), Some(&retitled)).is_ok());
        // A call that only looks is not held to the window, and it keeps the lease alive.
        let desk = session::Desk::new();
        assert!(go(&desk, "run_a", "Snapshot", &json!({}), Some(&seen_at(&notepad, GuardCaller::Agent)), Some(&mail)).is_ok());
        // The window in front now is judged as any other: the same program and window, under a title that is Aqua's own, is refused outright.
        let desk = session::Desk::new();
        let aqua_notes = Foreground { title: "Aqua notes".into(), ..notepad.clone() };
        let e = go(&desk, "run_a", "Click", &click, Some(&seen_at(&notepad, GuardCaller::Agent)), Some(&aqua_notes)).unwrap_err();
        assert!(e.code == ErrorCode::PermissionDenied && e.message.contains("approval prompts"), "{e:?}");
        assert_eq!(desk.holder_at(now), None);
        // No classification on record (nothing to compare with): the window is still judged, and the lease is still taken.
        let desk = session::Desk::new();
        assert!(go(&desk, "run_a", "Click", &click, None, Some(&notepad)).is_ok());
        assert!(go(&session::Desk::new(), "run_a", "Click", &click, None, Some(&aqua_notes)).is_err());
    }

    /// A page that went on to a payment step while the person was deciding is the same window, so only the step says it changed: the call is refused, unless the question that
    /// was put is a question about this call already.
    #[test]
    fn a_window_that_became_a_sign_in_or_payment_step_since_the_call_was_judged_is_refused_unless_a_question_about_it_was_already_put() {
        let page = Foreground { exe: "contoso.exe".into(), title: "Contoso".into(), window: 7 };
        let paying = Foreground { title: "Checkout - Contoso".into(), ..page.clone() };
        let step = critical::critical_point(&paying.title, "").expect("the title says checkout");
        let settings = Settings::Fixed { full_access: false, tiers: tiers::Policy::Strict };
        let now = Instant::now();
        let click = json!({"loc": [1, 1]});
        let judged = |asked: bool, caller: GuardCaller| Seen { key: "k".into(), front: Some(page.clone()), caller, asked };
        let go = |desk: &session::Desk, seen: &Seen, front: &Foreground, step: Option<&critical::Critical>| prepare_call(desk, "Click", &click, "run_a", None, Some(seen), Some(front), step, settings, now);
        // Judged on a plain page, the same window is at the payment step now: nothing is sent, and the desktop is not taken.
        let desk = session::Desk::new();
        let e = go(&desk, &judged(false, GuardCaller::Agent), &paying, Some(&step)).unwrap_err();
        assert!(e.code == ErrorCode::Conflict && e.message.contains("payment step") && e.message.contains("not put"), "{e:?}");
        assert_eq!(desk.holder_at(now), None);
        // The same page, not at a step: on it goes.
        assert!(go(&session::Desk::new(), &judged(false, GuardCaller::Agent), &paying, None).is_ok());
        // Judged as a question already, and answered: the person said yes to a question about this call.
        assert!(go(&session::Desk::new(), &judged(true, GuardCaller::Agent), &paying, Some(&step)).is_ok());
        // The voice is not asked at a step: refused outright.
        let e = go(&session::Desk::new(), &judged(false, GuardCaller::Voice), &paying, Some(&step)).unwrap_err();
        assert_eq!(e.code, ErrorCode::PermissionDenied, "{e:?}");
        // A call that only looks is not held to it.
        assert!(prepare_call(&session::Desk::new(), "Snapshot", &json!({}), "run_a", None, Some(&judged(false, GuardCaller::Agent)), Some(&paying), Some(&step), settings, now).is_ok());
    }

    #[test]
    fn a_call_takes_the_desktop_for_its_session_and_is_refused_with_the_name_of_the_session_that_has_it() {
        let notepad = Foreground { exe: "notepad.exe".into(), title: "Untitled".into(), window: 1 };
        let seen = Seen { key: "k".into(), front: Some(notepad.clone()), caller: GuardCaller::Agent, asked: false };
        let settings = Settings::Fixed { full_access: false, tiers: tiers::Policy::Strict };
        let now = Instant::now();
        let click = json!({"loc": [1, 1]});
        let desk = session::Desk::new();
        desk.claim_at("run_a", now).unwrap();
        let e = prepare_call(&desk, "Click", &click, "run_b", None, Some(&seen), Some(&notepad), None, settings, now).unwrap_err();
        assert!(e.code == ErrorCode::PermissionDenied && e.message.contains("another session (run_a)"), "{e:?}");
        assert_eq!(desk.holder_at(now).as_deref(), Some("run_a"), "the first session still has it");
        // A look by the second does not take it from the first, and does not fail.
        assert!(prepare_call(&desk, "Snapshot", &json!({}), "run_b", None, None, Some(&notepad), None, settings, now).is_ok());
        assert_eq!(desk.holder_at(now).as_deref(), Some("run_a"));
        // The owner keeps it.
        assert!(prepare_call(&desk, "Click", &click, "run_a", None, Some(&seen), Some(&notepad), None, settings, now).is_ok());
        // The voice is a session of its own, named by its kind when no run is behind it, and a name given on the thread is used when there is no run.
        let voice = Seen { caller: GuardCaller::Voice, ..seen.clone() };
        let e = prepare_call(&desk, "Type", &json!({"text": "hi"}), "", None, Some(&voice), Some(&notepad), None, settings, now).unwrap_err();
        assert!(e.message.contains("another session (run_a)"), "{e:?}");
        let free = session::Desk::new();
        assert!(prepare_call(&free, "Type", &json!({"text": "hi"}), "", None, Some(&voice), Some(&notepad), None, settings, now).is_ok());
        assert_eq!(free.holder_at(now).as_deref(), Some("windows:voice"));
        let free = session::Desk::new();
        assert!(prepare_call(&free, "Click", &click, "", Some("named_on_the_thread".into()), Some(&seen), Some(&notepad), None, settings, now).is_ok());
        assert_eq!(free.holder_at(now).as_deref(), Some("named_on_the_thread"));
    }

    /// Through the provider's own `call`: a window that changed since it was judged, and a desktop that another session took meanwhile, are refused before the server is reached.
    #[test]
    fn a_call_is_refused_before_the_server_is_reached_when_the_window_changed_or_another_session_took_the_desktop_since_it_was_judged() {
        let front = Arc::new(Mutex::new(Some(window("notepad.exe", "Untitled"))));
        let reader = front.clone();
        let p = WindowsUiProvider::isolated(client(), Arc::new(move || reader.lock().unwrap().clone()), false, tiers::Policy::Strict);
        let exec = ExecCtx { request_id: "r".into(), session_id: "s".into(), agent_run_id: "run_a".into(), cancel: Arc::new(AtomicBool::new(false)), timeout_ms: 1000, run_holds_outside_text: false, shown: None, progress: None };
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let click = json!({"loc": [1, 1]});
        let ctx = asked_as(Caller::Agent, "run_a");
        // The window changes between the question and the call.
        p.classify_for("win.click", &click, &ctx).unwrap();
        *front.lock().unwrap() = Some(window("outlook.exe", "Inbox"));
        let e = rt.block_on(p.call("win.click", click.clone(), &exec)).unwrap_err();
        assert_eq!(e.code, ErrorCode::Conflict, "{e:?}");
        assert!(!p.client.lock().unwrap().is_running(), "the server was started for a call that is not made");
        // Another session takes the desktop between the question and the call.
        *front.lock().unwrap() = Some(window("notepad.exe", "Untitled"));
        p.classify_for("win.click", &click, &ctx).unwrap();
        p.screen.desk().claim_at("run_x", Instant::now()).unwrap();
        let e = rt.block_on(p.call("win.click", click.clone(), &exec)).unwrap_err();
        assert!(e.code == ErrorCode::PermissionDenied && e.message.contains("another session (run_x)"), "{e:?}");
        assert!(!p.client.lock().unwrap().is_running());
        // A name that is not a desktop tool is not found, and the server is not started for it either.
        let e = rt.block_on(p.call("win.nothing", json!({}), &exec)).unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);
        assert!(!p.client.lock().unwrap().is_running());
    }

    #[test]
    fn the_desktop_takes_part_in_the_door_and_a_skills_list_may_name_its_tools_as_the_model_does() {
        let p = provider();
        assert!(p.door_applies());
        assert_eq!(p.restriction_names("win.click"), vec!["Click".to_string()]);
        assert_eq!(p.restriction_names("win.display_inventory"), vec!["DisplayInventory".to_string()]);
        assert!(p.restriction_names("win.nothing").is_empty() && p.restriction_names("fs.read").is_empty());
        for m in p.manifests() {
            assert_eq!(p.restriction_names(&m.name).len(), 1, "{}", m.name);
        }
        // The door reads a desktop tool's name as the guard does: in any case, and a name with anything else about it is not a tool.
        assert_eq!(host_name_for_model_name("Click"), Some("win.click"));
        assert_eq!(host_name_for_model_name("click"), Some("win.click"));
        assert_eq!(host_name_for_model_name("DISPLAYINVENTORY"), Some("win.display_inventory"));
        assert_eq!((host_name_for_model_name(" Click"), host_name_for_model_name("PowerShell"), host_name_for_model_name("")), (None, None, None));
        assert_eq!(host_name_for_wire("click"), None, "the exact lookup stays exact");
    }

    #[test]
    fn classify_refuses_shells_before_anything_runs() {
        let p = provider();
        let err = p.classify("win.app", &json!({"mode": "launch", "name": "PowerShell"})).unwrap_err();
        assert_eq!(err.code, ErrorCode::PermissionDenied);
        let err = p.classify("win.shortcut", &json!({"shortcut": "win+r"})).unwrap_err();
        assert_eq!(err.code, ErrorCode::PermissionDenied);
        assert!(p.classify("win.snapshot", &json!({})).is_ok());
    }
}

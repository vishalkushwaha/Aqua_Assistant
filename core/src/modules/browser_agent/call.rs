//! The one dispatch of the loop: every call the model makes comes through `call_named_tool`, and the order of what is asked before a call runs is fixed there (card OA3-05, move only).

use super::*;

/// With `AQUA_DOOR=on`, a browser or desktop call goes through the tool host (card R6-08): `None` when the door is not on, so the caller takes the route it has always taken.
fn door_route(tool: &str, arguments: &Value) -> Option<String> {
    crate::agent_tools::door_call(crate::door_shadow::DoorMode::from_env(), crate::agent_tools::host().map(|h| &**h), tool, arguments.clone(), door_caller(), &crate::agent_runs::scope::current_run())
}

/// Who is asking, as the tool host names it.
fn door_caller() -> aqua_tool_host::Caller {
    match windows_caller_now() {
        WindowsCaller::Voice => aqua_tool_host::Caller::Voice,
        WindowsCaller::Api => aqua_tool_host::Caller::Api,
        WindowsCaller::Agent => aqua_tool_host::Caller::Agent,
    }
}

/// The agent loop's one dispatch: every call the model makes comes here, and the order of what is asked is fixed (a loaded skill's list, the limits of the run, then the route the
/// tool takes). `pub(crate)` for the tests that run it with a host, a run and the limits of their own (card OA3-01); nothing else calls it from outside this module.
pub(crate) fn call_named_tool(
    tool: &str,
    arguments: Value,
    tools: &[McpTool],
    browser: &Option<BrowserHandle>,
    windows: &Option<Arc<Mutex<StdioMcpClient>>>,
) -> Result<(String, String)> {
    // A skill that is loaded narrows the run to the tools its list names, for every tool and not only the ones that go through the host (card R6-16): a click in the browser,
    // a window of the desktop and a search of the computer are refused here, in one place, in the words the host uses.
    if let Some(refused) = crate::agent_runs::scope::skill_refusal(tool) {
        return Ok(("guard".into(), refused));
    }
    // The limits of one run (card R7-18, `autonomy`): a run that has used its calls, its time, or the files and bytes it may change is not given the call, whichever route it would take.
    if let Some(stopped) = crate::agent_runs::scope::limit_stop(tool, &arguments) {
        return Ok(("guard".into(), stopped));
    }
    // `local_open_url` and `local_open` start something on this computer: the repeat of one that was running when Aqua stopped is the person's to decide, like any other.
    if tool.eq_ignore_ascii_case("local_open_url") || tool.eq_ignore_ascii_case("local_open") {
        if let Some(refusal) = unknown_repeat_refusal(tool, tool, &arguments) {
            return Ok(refusal);
        }
    }
    if tool.eq_ignore_ascii_case("local_open_url") {
        let url = arguments
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("local_open_url requires url"))?;
        // Same rules and the same question as browser_navigate: "ask" sites are not silently allowed, and after outside text was read an address nobody listed asks even
        // under full access (the address can carry what the run knows).
        if let Some(refused) = ask_before_acting_when_tainted(browser, crate::agent_runs::scope::run_is_tainted(), "browser", tool, &arguments) {
            return Ok(("guard".into(), refused));
        }
        if windows_caller_now() == WindowsCaller::Voice && aqua_mcp_adapters::browser::url_targets_local_machine(url) {
            return Ok(("guard".into(), "ERROR [POLICY_TOOL_BLOCKED] local_open_url: a voice request cannot open pages on this computer or its network.".to_string()));
        }
        if let Err(reason) = authorize_local_open(browser, url) {
            return Ok(("local".into(), format!("ERROR [POLICY_BLOCKED_ORIGIN] local_open_url: {reason}")));
        }
        let named = arguments.get("browser").and_then(|v| v.as_str());
        return match open_url_locally(url, named) {
            Ok(obs) => Ok(("local".into(), obs)),
            Err(e) => Ok(("local".into(), format!("ERROR opening {url}: {e:#}"))),
        };
    }
    // Finding a program or file on this computer, and opening one of the results the user chose.
    if tool.eq_ignore_ascii_case("local_find") {
        let query = arguments.get("query").or_else(|| arguments.get("name")).and_then(|v| v.as_str()).unwrap_or("").trim();
        if query.is_empty() {
            return Ok(("local".into(), "ERROR [INVALID_ARGUMENTS] local_find requires query (the name to look for)".to_string()));
        }
        info!("local_find ({} characters)", query.chars().count());
        set_searched_this_run(true);
        return Ok(("local".into(), aqua_mcp_adapters::local_files::find(query)));
    }
    if tool.eq_ignore_ascii_case("local_open") {
        if searched_this_run() {
            return Ok(("local".into(), "ERROR [NEEDS_USER_CHOICE] local_open: this request only searched. Do not open anything now: give the final answer listing what was found, by kind, and ask which one to open.".to_string()));
        }
        let target = arguments
            .get("target")
            .or_else(|| arguments.get("which"))
            .or_else(|| arguments.get("path"))
            .or_else(|| arguments.get("number"))
            .map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
            .unwrap_or_default();
        info!("local_open {target:?}");
        return Ok(("local".into(), match aqua_mcp_adapters::local_files::open(&target) {
            Ok(done) => done,
            Err(why) => format!("ERROR local_open: {why}"),
        }));
    }
    // A model's spelling of a host tool (`fs_read_file`) becomes the real name before anything else.
    if tool == "todo.write" {
        return Ok(("agent".into(), replace_checklist(&arguments)));
    }
    if tool == "agent.note" {
        return Ok(("agent".into(), session_run::write_note(&arguments)));
    }
    // A helper is started by the loop itself, which holds the planner it is asked through (card OA11-04): a call that reaches here (a recipe, a test) has no loop to start it.
    if tool == child::TOOL {
        return Ok(("guard".into(), format!("ERROR [UNAVAILABLE] {}: a helper is started by the agent loop of a coding run, not from here", child::TOOL)));
    }
    if tool == "agent.explore" {
        let used = explore_used();
        if !crate::modules::coding_driver::explore_allowed(used) {
            return Ok(("guard".into(), "ERROR [EXPLORE_CAP] agent.explore has already searched 8 times this run. Use a file you already found.".into()));
        }
        set_explore_used(used + 1);
        let pattern = arguments.get("pattern").and_then(|v| v.as_str()).unwrap_or("").trim();
        if pattern.is_empty() || pattern.len() > 200 {
            return Ok(("guard".into(), "ERROR [INVALID_ARGUMENTS] agent.explore requires pattern (1 to 200 characters). It only searches; it cannot write or run programs.".into()));
        }
        let Some(host) = crate::agent_tools::host() else {
            return Ok(("guard".into(), "ERROR [UNAVAILABLE] agent.explore: the tool host is not running".into()));
        };
        let mut args = json!({"pattern": pattern, "max_results": 20});
        if let Some(path) = arguments.get("path").and_then(|v| v.as_str()).filter(|p| !p.is_empty()) {
            args["path"] = json!(path);
        }
        let obs = crate::agent_tools::call(host, "fs.grep", args, door_caller(), &crate::agent_runs::scope::current_run());
        return Ok(("host".into(), format!("Explore (read only, one search):\n{obs}")));
    }
    if plan_mode_blocks(tool) || plan_run_blocks(tool) {
        return Ok(("guard".into(), format!("ERROR [POLICY_TOOL_BLOCKED] {tool}: plan mode can read and search, not change files or run programs.")));
    }
    let mut canonical = crate::agent_tools::canonical(tool, tools);
    // A host tool that was not shown for this goal is still the host's to decide on, not the desktop's (where it would be "unknown").
    let hidden_host_tool = canonical.is_none() && crate::agent_tools::router_v2() && {
        let coding = crate::agent_tools::is_coding_goal(&goal_for_context());
        canonical = crate::agent_tools::host().and_then(|h| crate::agent_tools::canonical_hidden(tool, h, tools, coding));
        canonical.is_some()
    };
    let announced = tool;
    let tool = canonical.as_deref().unwrap_or(tool);
    let source = if hidden_host_tool { "host" } else { pick_source(tool, tools) };
    // A call that was running when Aqua stopped, repeated in the run that goes on from it, on a route the host does not decide for: it is not
    // run again without the person (the host's own tools ask; these have no such question, so the run is told what to do instead).
    if source != "host" {
        if let Some(refusal) = unknown_repeat_refusal(announced, tool, &arguments) {
            return Ok(refusal);
        }
    }
    if source == "host" {
        let Some(host) = crate::agent_tools::host() else {
            return Ok(("guard".into(), format!("ERROR [UNAVAILABLE] {tool}: the tool host is not running")));
        };
        let caller = match windows_caller_now() {
            WindowsCaller::Voice => aqua_tool_host::Caller::Voice,
            WindowsCaller::Api => aqua_tool_host::Caller::Api,
            WindowsCaller::Agent => aqua_tool_host::Caller::Agent,
        };
        let run = crate::agent_runs::scope::current_run();
        let goal = goal_for_context();
        let round = planner_round();
        // A check the loop ran for an earlier call and did not record is not this call's.
        let _ = take_automatic_checks();
        if let Some(blocked) = crate::modules::coding_session::read_only_first_turn(&goal, tool, round) {
            return Ok(("guard".into(), blocked.to_string()));
        }
        if let Some(blocked) = crate::modules::coding_session::block_unread_change(&goal, tool, &arguments) {
            return Ok(("guard".into(), blocked));
        }
        // The version of the file the run last saw goes with an edit or an overwrite that did not name one: a file changed in between is refused by the tool (card OA6-01, AG-08).
        let mut arguments = arguments;
        crate::modules::coding_session::inject_expect_version(&goal, tool, &mut arguments);
        let (obs, data) = crate::agent_tools::call_detailed(host, tool, arguments.clone(), caller, &run);
        let mut text = crate::modules::coding_runner::after_host_data(tool, &arguments, &obs, &data);
        // The note of the folders the file just read is in, when the person trusted them (`AQUA_NESTED_NOTES`, card OA4-05, CU-11): once a run for each folder.
        if tool == "fs.read" && !obs.starts_with("ERROR") && crate::flags::is_on(&crate::flags::NESTED_NOTES) && crate::modules::run_profile::coding(&goal) {
            let path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(g) = crate::tool_host::project_guard() {
                if let Some(abs) = g.resolve(path, aqua_native_tools::fs::Op::Read).ok().map(|r| r.path) {
                    if let Some(note) = g.root_path_of(&abs).and_then(|root| crate::modules::coding_runner::nested_note_after_read(&run, &abs, &root, &crate::runtime::data_dir())) {
                        text.push_str("\n");
                        text.push_str(&note);
                    }
                }
            }
        }
        // The formatter after an edit (`AQUA_CODING_FORMAT`, off; card OA6-03, ED-08): the project's own formatter on the file the call changed, through the tool host, before the check so that the
        // check reads the file as it will stay. What it did is one line under the answer; it never fails the edit.
        if crate::flags::is_on(&crate::flags::CODING_FORMAT) && !obs.starts_with("ERROR") {
            if let Some(guard) = crate::tool_host::project_guard() {
                for line in crate::modules::coding_runner::format_after_edit(host, &guard, caller, &run, tool, &arguments, &data) {
                    text.push('\n');
                    text.push_str(&line);
                }
            }
        }
        if let Some(root) = project_roots().first() {
            // For a patch of several files, the first changed file that has a check: the same check an edit of that file gets.
            if let Some(path) = crate::modules::coding_runner::check_target(tool, &arguments, &data) {
                // The check runs in the project the file is in (card OA4-05, CU-04), with the file as an absolute path so that it is the same file from there; a file whose project is not found is checked
                // where it always was, in the first root.
                let (cwd, path) = match crate::tool_host::project_guard().and_then(|g| project_of_edit(&g, &path)) {
                    Some((dir, abs)) => (dir, abs.to_string_lossy().into_owned()),
                    None => (root.clone(), path),
                };
                if let Some(args) = crate::modules::coding_runner::host_check_request(tool, &path, &obs, &cwd) {
                    let check_obs = crate::agent_tools::call(host, "term.run", args.clone(), caller, &run);
                    text.push_str("\nCheck output:\n");
                    // With `AQUA_SHORT_CHECK_OUTPUT` the model sees the exit code and the first few complaints of a failed check; the step the loop records keeps the whole text.
                    // With `AQUA_DIAGNOSTICS` a failed check is shown as the errors it found, a block per file with the files this run changed first (card OA6-03, CU-10); a check that passed, that timed out
                    // or that printed no error that can be read is shown as it printed, as with the switch off.
                    let diagnostics = if crate::flags::is_on(&crate::flags::DIAGNOSTICS) { crate::modules::coding_runner::diagnostics_text(&args, &check_obs, &crate::modules::coding_session::files_this_run().1) } else { None };
                    let shown = diagnostics.or_else(|| if crate::flags::is_on(&crate::flags::SHORT_CHECK_OUTPUT) { crate::modules::code_verification::summarise_check_output(&check_obs) } else { None });
                    text.push_str(shown.as_deref().unwrap_or(&check_obs));
                    // With `AQUA_CHECK_STEPS` the check is a call of the run like any other: the loop records it as a step of its own, so that the gate that asks for a passing check after the last
                    // change sees it (card OA4-04, ED-15). Without it its output is only printed under the edit's, and a run that fixed a file and read the check say "passed" still has to run it
                    // again (the transcripts of `core/tests/fixtures/agent_scenarios/golden/` pin that, and a golden is the owner's to write again).
                    if crate::flags::is_on(&crate::flags::CHECK_STEPS) {
                        push_automatic_check(args, check_obs);
                    }
                }
            }
        }
        return Ok(("host".into(), text));
    }
    if source == "browser" {
        // Voice can look at a page but never act on it: a spoken request (or a TV that says the wake word) must
        // not be able to click, type, press Enter, upload or run script in a logged-in browser.
        if windows_caller_now() == WindowsCaller::Voice && !voice_may_use_browser_tool(tool, &arguments) {
            return Ok((
                "guard".into(),
                format!("ERROR [POLICY_TOOL_BLOCKED] {tool}: a voice request can read pages but not click, type or submit. Ask in the app instead."),
            ));
        }
        let Some(b) = browser else {
            bail!("no browser available for tool {tool}");
        };
        // The planner may add `"browser": "chrome"` to a browser call (that is an argument of `local_open_url`; these tools have none, and it used to be dropped, so
        // the page opened in Edge whatever the person asked for). It is what the person asked, so it is honoured here and never passed on to the tool.
        let (arguments, named) = take_browser_name(arguments);
        // Choosing the browser saves a setting and relaunches it, so only a run that has read nothing from the web may do it from a call: after a page has been read, a
        // call's argument may be what that page told the model to write. The person's own request is read at the start of the run (`requested_channel`), not here.
        let named = named.filter(|name| {
            let may = a_call_may_choose_the_browser(crate::agent_runs::scope::run_is_tainted());
            if !may {
                warn!("a browser call named \"{name}\" after the run had read the web: not switched");
            }
            may
        });
        if let Some(name) = named {
            match aqua_mcp_adapters::browser::BrowserChannel::parse(&name) {
                Some(channel) => {
                    if let Err(e) = b.use_channel(channel) {
                        return Ok(("guard".into(), format!("ERROR [BROWSER_UNAVAILABLE] {tool}: {e:#}. Tell the person; do not use another browser in its place.")));
                    }
                }
                None => return Ok(("guard".into(), format!("ERROR [INVALID_ARGUMENTS] {tool}: the browsers that can be driven are Chrome and Edge, not \"{name}\"."))),
            }
        }
        // The one door (card R6-08, `AQUA_DOOR=on`): the call goes through the tool host, which decides by the door's rule, runs the person's hooks and a loaded skill's list, asks,
        // masks and writes it in the signed ledger. The question below is not put as well: the host's replaces it.
        if let Some(text) = door_route(tool, &arguments) {
            note_browser_used();
            return Ok(("browser".into(), text));
        }
        if let Some(refused) = ask_before_acting_when_tainted(browser, crate::agent_runs::scope::run_is_tainted(), "browser", tool, &arguments) {
            return Ok(("guard".into(), refused));
        }
        // What the tool host would decide, asked before the call: the call changes what the run holds.
        let door = crate::agent_tools::one_door_preview(tool, &arguments, door_caller());
        note_browser_used();
        // Every browser call goes through the policy layer (sites, confirmations, audit).
        let obs = b.call(tool, arguments);
        // Whatever it brought back is text from the web: from here on this run's changes need a fresh answer.
        crate::agent_runs::scope::taint_current_run();
        let text = obs.to_agent_text();
        crate::agent_tools::one_door_compare(door, &text);
        return Ok(("browser".into(), text));
    }
    // The browser the browser tools drive is already open, under Aqua's control. Launching a browser with `App` for the same website opens a second, unrelated
    // window (the person's own, with their profile), which is what a small model does when a learned recipe or the word "Chrome" in the request suggests it.
    if tool.eq_ignore_ascii_case("App") && launches_a_browser(&arguments) && browser_used_in_this_run() {
        return Ok((
            "guard".into(),
            "ERROR [USE_BROWSER_TOOLS] App: the browser is already open and under Aqua's control (browser_navigate, browser_snapshot, browser_click). Launching it again would open a second, unrelated window. Continue with the browser tools, or answer from what the page showed.".to_string(),
        ));
    }
    let Some(client) = windows.clone() else {
        // Never fall back to another route: a tool with no owner is simply unavailable.
        return Ok((
            "guard".into(),
            format!("ERROR [INVALID_ARGUMENTS] {tool}: no such tool is available"),
        ));
    };
    // The one door (card R6-08, `AQUA_DOOR=on`): the desktop's tools are the host's too; the provider holds the guard's rules (the window, a sign-in or payment step, one session at
    // a time), and the host's question replaces the one below.
    if let Some(text) = door_route(tool, &arguments) {
        return Ok(("windows".into(), explain_missing_tool(tool, text)));
    }
    if let Some(refused) = ask_before_acting_when_tainted(browser, crate::agent_runs::scope::run_is_tainted(), "windows", tool, &arguments) {
        return Ok(("guard".into(), refused));
    }
    // The guard is the only caller of Windows-MCP: allowlist, argument rules, approvals, audit.
    let broker = browser.as_ref().map(|b| b.broker());
    let caller = windows_caller_now();
    let door = crate::agent_tools::one_door_preview(tool, &arguments, door_caller());
    // The run is the desktop's session: a second one that acts on the screen is refused while this one has it (card R6-34).
    let run = crate::agent_runs::scope::current_run();
    let _session = (!run.is_empty()).then(|| aqua_mcp_adapters::windows_mcp::session::as_owner(&run));
    let obs = WindowsGuard::global().call(&client, broker, tool, arguments, caller);
    // Window titles and screen text are untrusted too, and what is on the screen is the person's.
    crate::agent_runs::scope::taint_current_run();
    crate::agent_runs::scope::mark_current_run_private();
    let text = explain_missing_tool(tool, obs);
    crate::agent_tools::one_door_compare(door, &text);
    Ok(("windows".into(), text))
}

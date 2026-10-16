//! The tools the loop offers and how they are reached: starting the two MCP clients and collecting what they offer, the tools the loop answers itself, the provider a tool belongs to, the arguments of a title-only snapshot, and the observation that follows an action (card OA3-05, move only).

use super::*;

fn with_client<R>(
    client: &Arc<Mutex<StdioMcpClient>>,
    f: impl FnOnce(&mut StdioMcpClient) -> Result<R>,
) -> Result<R> {
    let mut guard = client
        .lock()
        .map_err(|_| anyhow::anyhow!("MCP client lock poisoned"))?;
    f(&mut guard)
}

pub(super) fn ensure_started(client: &Arc<Mutex<StdioMcpClient>>, label: &str) -> Result<()> {
    with_client(client, |c| {
        if !c.is_initialized() {
            c.start().with_context(|| format!("start {label}"))?;
        }
        Ok(())
    })
}

pub(super) fn collect_tools_shared(
    browser: Option<&BrowserHandle>,
    windows: Option<&Arc<Mutex<StdioMcpClient>>>,
    goal: &str,
) -> Result<Vec<McpTool>> {
    let mut tools = Vec::new();
    if let Some(b) = browser {
        // Policy-filtered tool list for the active browser backend (managed or extension).
        tools.extend(b.tools()?);
    }
    if let Some(w) = windows {
        with_client(w, |c| {
            if c.is_initialized() {
                let list = if c.cached_tools().is_empty() {
                    c.list_tools()?
                } else {
                    c.cached_tools().to_vec()
                };
                // Only the desktop UI tools; shell/file/registry tools never reach the model.
                tools.extend(filter_windows_tools(list));
            }
            Ok(())
        })?;
    }
    // Files, terminal, git, web, ssh, docker, k8s, terraform, cloud, databases, monitoring and
    // system tools: only the packs this goal mentions, and only when the host is running.
    // A practice run (unattended, started by the agent itself) gets the browser only: no files,
    // terminal, git, databases or anything else that could change something.
    if !crate::modules::agent_experience::is_practice() {
        if let Some(host) = crate::agent_tools::host() {
            tools.extend(crate::agent_tools::tools_for_goal(host, goal));
            // The tools the loop handles itself, when they are switched on and the goal calls for them (they need the host's `fs.grep` or nothing at all).
            let own = agent_tools_for(goal, &tools);
            tools.extend(own);
        }
        tools.extend(local_tools());
    }
    // The agent the run is (card OA11-04) shows only the tools it names, whichever way they came.
    child::narrow_shown(&mut tools);
    Ok(tools)
}

/// Marks the tools the agent loop handles itself in `McpTool::source` (`todo.write`, `agent.explore`): they are answered in `call_named_tool`, not by the tool host.
pub(crate) const AGENT_SOURCE: &str = "agent";

/// The tools the loop already handled and no list offered (card OA5-01; tasks AG-01, TL-02, WF-11). A prompt that names a tool the reply cannot name gets it called and refused, and
/// `todo.write` and `agent.explore` had handlers (`call_named_tool`) but were in no list, so the grammar left them out. With `AQUA_AGENT_TOOLS=on`:
///
/// * `todo.write` for a coding goal or one that wants a plan: the run keeps a checklist (at most 20 items, one doing), shown again at the end of every prompt;
/// * `agent.explore` for a coding goal that is shown `fs.grep`: one read-only search through the host, at most eight in a run.
///
/// Neither changes anything outside the run: `todo.write` has no effect but the checklist, and `agent.explore` is an `fs.grep` that the host decides on like any other. With the switch off
/// the prompt does not name them either (`planner_prompt_block_with` names only what is on offer).
pub(crate) fn agent_tools_for(goal: &str, shown: &[McpTool]) -> Vec<McpTool> {
    // `agent.note` is the loop's own tool too, but its switch is the coding session's (card OA10-04, CX-08): it exists for a coding run that has a session to write the note in.
    let note = session_run::attached() && crate::modules::run_profile::coding(goal);
    let own = crate::flags::is_on(&crate::flags::AGENT_TOOLS);
    // `agent.task` is the loop's own tool too, and its switch is the agents' (card OA11-04): it is on offer for a coding run outside a helper, when an agent may be a helper.
    let helper = child::tool_for(goal);
    if !own && !note && helper.is_none() {
        return Vec::new();
    }
    // The line the model reads is cut at 80 characters for a tool that is not the host's (`tool_lines`): the arguments and the point of the tool have to fit.
    let tool = |name: &str, description: &str, schema: Value| McpTool { name: name.into(), description: description.into(), input_schema: schema, source: Some(AGENT_SOURCE.into()) };
    let coding = crate::modules::run_profile::coding(goal);
    let mut out = Vec::new();
    if note {
        out.push(tool(
            "agent.note",
            "(text) Keep a note of at most 160 characters for the next goal in this project.",
            json!({"type": "object", "properties": {"text": {"type": "string", "maxLength": 160}}, "required": ["text"]}),
        ));
    }
    out.extend(helper);
    if !own {
        return out;
    }
    if coding || crate::modules::coding_session::wants_plan(goal) {
        out.push(tool(
            "todo.write",
            "(items) Keep a checklist: [{text, status pending|doing|done}], one doing.",
            json!({"type": "object", "properties": {"items": {"type": "array", "maxItems": 20, "items": {"type": "object", "properties": {"text": {"type": "string", "maxLength": 80}, "status": {"type": "string", "enum": ["pending", "doing", "done"]}}, "required": ["text"]}}}, "required": ["items"]}),
        ));
    }
    if coding && shown.iter().any(|t| t.name == "fs.grep" && t.source.as_deref() == Some(crate::agent_tools::SOURCE)) {
        out.push(tool(
            "agent.explore",
            "(pattern, path?) One read-only search of the code, at most 8 in a run.",
            json!({"type": "object", "properties": {"pattern": {"type": "string", "maxLength": 200}, "path": {"type": "string"}}, "required": ["pattern"]}),
        ));
    }
    out
}

/// The two tools for finding a program or file on this computer and opening what the user picks. They are handled
/// in `call_named_tool` (see `aqua_mcp_adapters::local_files`).
pub(crate) fn local_tools() -> Vec<McpTool> {
    let tool = |name: &str, description: &str, schema: Value| McpTool {
        name: name.into(),
        description: description.into(),
        input_schema: schema,
        source: Some("local".into()),
    };
    vec![
        tool(
            "local_find",
            "Search this computer by name for programs, games, installers, shortcuts and folders (e.g. query \"Need for Speed\"). Returns a numbered list with each item's kind. Tell the user what was found and ask which to open.",
            json!({"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}),
        ),
        tool(
            "local_open",
            "Open an item from the last local_find, as double-clicking it would: target is its number, or a kind (\"executable\", \"installer\", \"shortcut\", \"folder\"). Only for something the user asked to open or run.",
            json!({"type": "object", "properties": {"target": {"type": "string"}}, "required": ["target"]}),
        ),
    ]
}

pub(super) fn pick_source(tool: &str, tools: &[McpTool]) -> &'static str {
    if let Some(meta) = tools.iter().find(|t| t.name == tool) {
        match meta.source.as_deref() {
            Some("windows") => return "windows",
            Some("browser") => return "browser",
            Some(crate::agent_tools::SOURCE) => return "host",
            _ => {}
        }
    }
    let lower = tool.to_ascii_lowercase();
    if lower.starts_with("browser_") || lower.starts_with("aqua_browser_") {
        return "browser";
    }
    "windows"
}

/// Title-only desktop questions do not need the UI tree. Full Snapshot stays the default
/// unless the goal asks only for the front window title, or the caller already set the flag.
pub(super) fn snapshot_arguments_for_goal(goal: &str, arguments: Value) -> Value {
    let mut arguments = if arguments.is_object() { arguments } else { json!({}) };
    let explicit = arguments.get("window_metadata_only").and_then(|v| v.as_bool());
    if explicit.is_some() {
        return arguments;
    }
    let goal = goal.to_ascii_lowercase();
    let title_only = (goal.contains("front window") || goal.contains("foreground window") || goal.contains("focused window"))
        && goal.contains("title")
        && !["click", "type ", "button", "element", "scroll", "shortcut"].iter().any(|word| goal.contains(word));
    if title_only {
        if let Some(obj) = arguments.as_object_mut() {
            obj.insert("window_metadata_only".into(), json!(true));
            obj.entry("use_vision").or_insert(json!(false));
        }
    }
    arguments
}

/// After UI-changing actions, pull a cheap/rich observation for the next reason step.
pub(super) fn auto_observe_order(after_tool: &str) -> &'static [&'static str] {
    if after_tool.to_ascii_lowercase().starts_with("browser_") {
        &["browser_snapshot", "Snapshot", "Screenshot"]
    } else {
        &["Snapshot", "Screenshot", "browser_snapshot"]
    }
}

pub(super) fn maybe_auto_observe(
    tools: &[McpTool],
    browser: &Option<BrowserHandle>,
    windows: &Option<Arc<Mutex<StdioMcpClient>>>,
    after_tool: &str,
) -> Option<(String, String, String)> {
    // After a browser action the thing that changed is the page, so read the page. The desktop
    // snapshot lists every open window (titles of unrelated apps and documents) and says nothing
    // about the page: the planner ended up answering from it, and the answer showed those titles.
    // Desktop actions keep the old order: Windows Snapshot → Screenshot → browser_snapshot.
    for &name in auto_observe_order(after_tool) {
        if !tools.iter().any(|t| t.name == name) {
            continue;
        }
        let args = if name == "Snapshot" {
            // Prefer text UI tree; keep payload smaller for the LLM.
            json!({ "use_vision": false })
        } else {
            json!({})
        };
        match call_named_tool(name, args, tools, browser, windows) {
            Ok((label, obs)) if !obs.starts_with("ERROR") && obs.trim().len() > 8 => {
                // The prompt condenses and caps this again; cutting it here first dropped the part of
                // a long page (search results, quotes) the answer was in.
                return Some((name.to_string(), label, truncate(&obs, 6000)));
            }
            Ok((_, obs)) => {
                warn!("Auto-observe {name} weak result: {}", truncate(&obs, 200));
            }
            Err(e) => {
                warn!("Auto-observe {name} failed: {e:#}");
            }
        }
    }
    None
}

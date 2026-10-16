//! Which tools a run is shown for its goal: the tools of its packs, read tools first and capped, the tools of the servers the person added, the command tools, the search for tools and the skills, and what a command or an agent narrows; and the names a model writes for them, the tools of a pack that was shown that it may still reach, and what a tool can do: change something, send something (move only: this was part of `agent_tools.rs`).

use super::*;

/// The tools that come first inside a pack, before the read-first, alphabetical order of the rest. Without it a pack of eleven
/// file tools showed `fs.list`, `fs.find`, `fs.read`, `fs.stat`, then `fs.copy` and `fs.delete`, and `fs.edit` and `fs.write`
/// fell off the end of the list: the one task a coding request is about, editing a file, was the one tool the model could not see.
pub(super) fn priority_of(pack: &str) -> &'static [&'static str] {
    match pack {
        "files" => &["fs.find", "fs.read", "fs.edit", "fs.write", "fs.grep", "fs.patch", "fs.list", "fs.stat"],
        "terminal" => &["term.run", "term.shell", "term.output", "term.start", "term.stop"],
        // `git.commit` comes before `git.log` (card OA5-01): it stages what it is told to (`paths`, `all`) and commits, so the tools a commit goal needs are the first three, and the log is the one to give way.
        "git" => &["git.status", "git.diff", "git.commit", "git.log", "git.add"],
        "web" => &["web.search", "web.read", "web.fetch"],
        "docs" => &["doc.read", "doc.search"],
        "flows" => &["flows.list", "flows.run", "flows.status"],
        _ => &[],
    }
}

/// One line the small model can act on: `(path, offset?) description`.
fn describe(m: &ToolManifest) -> String {
    let sig = m.signature();
    let args = sig.split_once('(').map(|(_, rest)| format!("({}", rest)).unwrap_or_else(|| "()".into());
    format!("{args} {}", m.description)
}

/// The host tools to show for `goal`: read tools first inside each pack, capped.
///
/// A run that was started by a command (card OA11-02) has options: the packs it is shown are the command's own (or, when it names none, the packs of the tools it allows), and only the tools its
/// `allowed-tools` name are shown at all. The host refuses the others whatever is shown (`ToolHost::restrict_run`); this keeps a small model from being shown what it may not call.
pub fn tools_for_goal(host: &ToolHost, goal: &str) -> Vec<McpTool> {
    let run = current_run();
    let options = options_of(&run);
    let allowed: &[String] = options.as_ref().map(|o| o.allowed_tools.as_slice()).unwrap_or(&[]);
    // A list that names everything (`*`) narrows nothing.
    let allowed: &[String] = if allowed.iter().any(|a| a == "*") { &[] } else { allowed };
    // The agent the run is (card OA11-04): the tools its definition names narrow what is shown, as a command's list does, and its packs are the ones shown when the command names none.
    let agent = crate::agents::of_run(&run);
    let agent_allowed: &[String] = agent.as_ref().map(|a| a.allowed_tools.as_slice()).unwrap_or(&[]);
    let agent_allowed: &[String] = if agent_allowed.iter().any(|a| a == "*") { &[] } else { agent_allowed };
    let forced = options.as_ref().and_then(|o| forced_packs_for(&o.forced_packs, allowed)).or_else(|| agent.as_ref().and_then(|a| forced_packs_for(&a.packs, agent_allowed)));
    let mut tools = tools_for_goal_forced(host, goal, router_v2(), forced.as_deref(), if allowed.is_empty() { agent_allowed } else { allowed });
    // The search for tools reaches the rest of the catalogue (off unless AQUA_TOOL_SEARCH=on).
    if crate::tool_search::enabled() && !tools.iter().any(|t| t.name == "tools.search") {
        if let Some(m) = host.manifests().into_iter().find(|m| m.name == "tools.search") {
            tools.push(McpTool { name: m.name.clone(), description: describe(&m), input_schema: m.input_schema.clone(), source: Some(SOURCE.into()) });
        }
    }
    // The servers the user added (`mcp.json`) and the ones of a trusted repository: their tools are shown for a goal that names the server ("add a note in notes"), or as the server's `expose`
    // says, to the runs in its scope, up to eight (card OA11-03; `mcp_exposure`).
    for m in crate::mcp_exposure::tools_for(host, goal, &run).tools {
        if !tools.iter().any(|x| x.name == m.name) {
            tools.push(McpTool { name: m.name.clone(), description: format!("{} {}", m.typed_signature().split_once('(').map(|(_, rest)| format!("({rest}")).unwrap_or_default(), m.description), input_schema: m.input_schema.clone(), source: Some(SOURCE.into()) });
        }
    }
    // The command tools of the person's file and of a trusted project (off unless `AQUA_COMMAND_TOOLS=on`): the ones the goal names, and for a coding goal the rest, at most four.
    for t in command_tools_shown(host, goal, &run) {
        if !tools.iter().any(|x| x.name == t.name) {
            tools.push(t);
        }
    }
    // The user's own skills: `skill.load` is shown only for a goal that one of them fits (and the prompt lists which).
    if crate::skills::any_fit(goal) && !tools.iter().any(|t| t.name == "skill.load") {
        if let Some(m) = host.manifests().into_iter().find(|m| m.name == "skill.load") {
            tools.push(McpTool { name: m.name.clone(), description: describe(&m), input_schema: m.input_schema.clone(), source: Some(SOURCE.into()) });
        }
    }
    // The other files of a skill that fits (a template, a checklist): `skill.file`, when it is on (`AQUA_SKILLS_V2`) and one of those skills has files.
    if crate::skills::any_files_fit(goal) && !tools.iter().any(|t| t.name == "skill.file") {
        if let Some(m) = host.manifests().into_iter().find(|m| m.name == "skill.file") {
            tools.push(McpTool { name: m.name.clone(), description: describe(&m), input_schema: m.input_schema.clone(), source: Some(SOURCE.into()) });
        }
    }
    // A command that narrows the run: what it does not allow is not shown (skill.load is the host's own exception and stays when a skill fits).
    if !allowed.is_empty() {
        tools.retain(|t| t.name == "skill.load" || t.name == "skill.file" || allowed.iter().any(|p| aqua_tool_host::tool_pattern_matches(p, &t.name)));
    }
    // The same for the tools of the agent the run is (a tool has to be in every list).
    if !agent_allowed.is_empty() {
        tools.retain(|t| t.name == "skill.load" || t.name == "skill.file" || agent_allowed.iter().any(|p| aqua_tool_host::tool_pattern_matches(p, &t.name)));
    }
    tools
}

/// The command tools to show for a goal: the ones the goal names (by the short name or the whole name), then, for a coding goal, the others, at most [`crate::command_tools::MAX_SHOWN`].
fn command_tools_shown(host: &ToolHost, goal: &str, run: &str) -> Vec<McpTool> {
    let all = crate::command_tools::tools_of(host, run);
    if all.is_empty() {
        return Vec::new();
    }
    let g = goal.to_ascii_lowercase();
    let named = |t: &aqua_native_tools::term::custom::CustomTool| {
        let short = t.name.split_once('.').map_or(t.name.as_str(), |(_, s)| s);
        has_word(&g, &t.name) || has_word(&g, short) || has_word(&g, &short.replace('_', " "))
    };
    let coding = is_coding_goal(goal);
    let mut order: Vec<&Arc<aqua_native_tools::term::custom::CustomTool>> = all.iter().filter(|t| named(t)).collect();
    if coding {
        order.extend(all.iter().filter(|t| !named(t)));
    }
    order
        .into_iter()
        .take(crate::command_tools::MAX_SHOWN)
        .map(|t| McpTool { name: t.name.clone(), description: t.shown_line(), input_schema: t.schema(), source: Some(SOURCE.into()) })
        .collect()
}

/// The packs a command's run is shown: the packs it names (those that exist), else the packs of the tools it allows. `None`: the words of the goal decide.
fn forced_packs_for(named: &[String], allowed: &[String]) -> Option<Vec<&'static str>> {
    let packs = crate::tool_providers::packs();
    let mut out: Vec<&'static str> = named.iter().filter_map(|n| packs.iter().find(|p| p.name == n.to_ascii_lowercase()).map(|p| p.name)).collect();
    if out.is_empty() {
        out = packs
            .iter()
            .filter(|p| allowed.iter().any(|a| p.prefixes.iter().any(|pre| a.starts_with(pre) || pre.starts_with(a.trim_end_matches('*')))))
            .map(|p| p.name)
            .collect();
    }
    out.dedup();
    out.truncate(MAX_FORCED_PACKS);
    (!out.is_empty()).then_some(out)
}

/// The most packs a command may name.
const MAX_FORCED_PACKS: usize = 4;

#[cfg(test)]
pub(super) fn tools_for_goal_with(host: &ToolHost, goal: &str, v2: bool) -> Vec<McpTool> {
    tools_for_goal_forced(host, goal, v2, None, &[])
}

/// [`tools_for_goal_with`] for a run that a command started: `forced` are the packs it is shown instead of the ones the goal's words pick (the coding set is not pinned then), and
/// `allowed` the tools it may call (a tool outside them is left out of each pack before the pack's places are counted).
fn tools_for_goal_forced(host: &ToolHost, goal: &str, v2: bool, forced: Option<&[&'static str]>, allowed: &[String]) -> Vec<McpTool> {
    let packs = match forced {
        Some(f) => f.to_vec(),
        None => route_with(goal, v2),
    };
    if packs.is_empty() {
        return Vec::new();
    }
    let all = host.manifests();
    let coding = forced.is_none() && v2 && crate::modules::run_profile::routed_as_work(goal);
    // Each pack gets an equal share of the cap; whatever a small pack leaves unused goes to the others.
    let quota = MAX_HOST_TOOLS / packs.len();
    let mut lists: Vec<Vec<McpTool>> = Vec::new();
    for pack in packs {
        let Some(p) = crate::tool_providers::packs().iter().find(|p| p.name == pack) else { continue };
        let mut mine: Vec<&ToolManifest> = all.iter().filter(|m| p.prefixes.iter().any(|pre| m.name.starts_with(pre))).collect();
        if !allowed.is_empty() {
            mine.retain(|m| allowed.iter().any(|a| aqua_tool_host::tool_pattern_matches(a, &m.name)));
        }
        // A coding goal is never shown the tools of `coding_driver::DENY` (push, reset, remote shells...): they are left out before the pack's places are counted, so that none is spent on them.
        if coding {
            mine.retain(|m| !crate::modules::coding_driver::denied(&m.name));
        }
        let first = if v2 { priority_of(p.name) } else { &[] };
        let rank = |m: &ToolManifest| first.iter().position(|n| *n == m.name).unwrap_or(usize::MAX);
        mine.sort_by(|a, b| rank(a).cmp(&rank(b)).then(b.annotations.read_only.cmp(&a.annotations.read_only)).then(a.name.cmp(&b.name)));
        lists.push(mine.into_iter().map(|m| McpTool { name: m.name.clone(), description: describe(m), input_schema: m.input_schema.clone(), source: Some(SOURCE.into()) }).collect());
    }
    let mut out: Vec<McpTool> = Vec::new();
    let mut rest: Vec<McpTool> = Vec::new();
    for l in lists {
        let mut it = l.into_iter();
        out.extend(it.by_ref().take(quota));
        rest.extend(it);
    }
    out.extend(rest.into_iter().take(MAX_HOST_TOOLS - out.len()));
    if coding {
        pin_coding_tools(&mut out, &all, &crate::modules::coding_session::extra_tools_for(goal));
        out.retain(|t| !crate::modules::coding_driver::denied(&t.name));
        if coding_packs_on() {
            add_coding_pack(&mut out, &all, goal);
        }
    }
    out
}

/// The tool as the agent is shown it.
pub(super) fn as_shown(m: &ToolManifest) -> McpTool {
    McpTool { name: m.name.clone(), description: describe(m), input_schema: m.input_schema.clone(), source: Some(SOURCE.into()) }
}

/// Does `text` (lower case) contain `word` as a word of its own?
pub(super) fn has_word(text: &str, word: &str) -> bool {
    !word.is_empty() && text.match_indices(word).any(|(i, _)| {
        let before = text[..i].chars().next_back().map_or(true, |c| !c.is_alphanumeric());
        let after = text[i + word.len()..].chars().next().map_or(true, |c| !c.is_alphanumeric());
        before && after
    })
}

/// The host's tools with these names, as the agent is shown them.
pub fn tools_named(host: &ToolHost, names: &[String]) -> Vec<McpTool> {
    host.manifests()
        .into_iter()
        .filter(|m| names.contains(&m.name))
        .map(|m| McpTool { name: m.name.clone(), description: format!("{} {}", m.typed_signature().split_once('(').map(|(_, rest)| format!("({rest}")).unwrap_or_default(), m.description), input_schema: m.input_schema.clone(), source: Some(SOURCE.into()) })
        .collect()
}

/// Resolve the name a model wrote to a shown tool. `None` when nothing matches.
pub fn canonical(name: &str, tools: &[McpTool]) -> Option<String> {
    let hosts: Vec<&McpTool> = tools.iter().filter(|t| t.source.as_deref() == Some(SOURCE)).collect();
    let mut n = name.trim().to_lowercase();
    for prefix in ["host.", "host_", "aqua.", "aqua_"] {
        if let Some(rest) = n.strip_prefix(prefix) {
            n = rest.to_string();
        }
    }
    let find = |c: &str| hosts.iter().find(|t| t.name == c).map(|t| t.name.clone());
    if let Some(t) = find(&n) {
        return Some(t);
    }
    // `fs_read_file`, `fs-read-file`, `fs.read.file`: the first separator is the domain dot and the
    // rest is joined with underscores.
    let (domain, rest) = n.split_once(['.', '_', '-'])?;
    find(&format!("{domain}.{}", rest.replace(['.', '-'], "_")))
}

/// Like [`canonical`], for a tool of the host that the list cut off: a pack that was shown (the files pack) holds more tools than fit, and
/// the model may name one that did not (`fs.edit` behind `fs.copy` and `fs.delete`). The call then goes to the host, which decides, asks and
/// refuses as it does for any call.
///
/// Only a tool of a pack that **was shown for this goal** counts. A goal that was shown no files pack cannot reach `fs.*` by naming it, and
/// one that was shown only files and terminal tools cannot reach `ssh.*` or `docker.*`: choosing what is shown is part of keeping a small
/// model, and a page that tries to talk it into a tool, away from tools the task has no use for.
///
/// `coding`: the run is a coding run. The tools of `coding_driver::DENY` (push, reset, remote shells...) are left out of what it is shown, and naming one does not bring it back either (card OA5-01,
/// TM-08: they were only left out of the list, so a model that wrote `git.push` by name, with the reply not constrained to the list, reached the host's question for it).
pub fn canonical_hidden(name: &str, host: &ToolHost, shown: &[McpTool], coding: bool) -> Option<String> {
    let all: Vec<McpTool> = host
        .manifests()
        .into_iter()
        .map(|m| McpTool { name: m.name, description: String::new(), input_schema: Value::Null, source: Some(SOURCE.into()) })
        .collect();
    let resolved = canonical(name, &all)?;
    if coding && crate::modules::coding_driver::denied(&resolved) {
        return None;
    }
    let domain = |n: &str| n.split('.').next().unwrap_or("").to_string();
    let wanted = domain(&resolved);
    shown.iter().filter(|t| t.source.as_deref() == Some(SOURCE)).any(|t| domain(&t.name) == wanted).then_some(resolved)
}

/// Is `tool` one the host owns (whether or not it is shown in this run)?
pub fn is_host_tool(host: Option<&ToolHost>, tool: &str) -> bool {
    host.is_some_and(|h| h.manifests().iter().any(|m| m.name == tool))
}

pub fn is_read_only(host: Option<&ToolHost>, tool: &str) -> bool {
    host.is_some_and(|h| h.manifests().iter().any(|m| m.name == tool && m.annotations.read_only))
}

/// The desktop and browser tools that only look. Exactly these names and no others: a name that merely contains a word like "read" or "get" says
/// nothing about what the tool does, and a wrong "only looks" would let a click or a send be repeated without anyone being asked.
const ONLY_LOOK: [&str; 8] = ["snapshot", "screenshot", "browser_snapshot", "browser_take_screenshot", "browser_find", "aqua_browser_extract", "local_find", "browser_tabs_list"];

/// Can this tool change something? A tool of the host unless it declares itself read-only (its classes depend on the arguments, and the arguments
/// of a call that stopped are not kept whole, so "not marked read-only" is the safe reading); the others (the desktop and browser tools) unless
/// they are one of the few known to only look.
pub fn can_change_something(host: Option<&ToolHost>, tool: &str) -> bool {
    if let Some(m) = host.and_then(|h| h.manifests().into_iter().find(|m| m.name == tool)) {
        return !m.annotations.read_only;
    }
    !ONLY_LOOK.contains(&tool.to_ascii_lowercase().as_str())
}

/// Does this tool reach out: the internet, another machine, another system? The host's tools by their classes (a read from the web reaches out too:
/// its address or query is data that leaves); the browser's tools by name; the desktop tools (clicks and typing) do not.
pub fn can_send_something(host: Option<&ToolHost>, tool: &str) -> bool {
    if let Some(m) = host.and_then(|h| h.manifests().into_iter().find(|m| m.name == tool)) {
        return m.classes.iter().any(|c| matches!(c, aqua_tool_host::PermClass::Network | aqua_tool_host::PermClass::ExternalEffect | aqua_tool_host::PermClass::Remote));
    }
    let t = tool.to_ascii_lowercase();
    t.starts_with("browser_") || t.starts_with("aqua_browser") || t == "local_open_url"
}

//! The tests of `modules::browser_agent::reachability_tests`, moved here from an inline module of `browser_agent.rs` (card OA3-05, move only): the path of every test is as it was.

use super::*;
use crate::modules::planner::schema::{action_schema_with, Extras, Mode};

fn mk(name: &str, source: &str) -> McpTool {
    McpTool { name: name.into(), description: format!("({name}) does it"), input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}), source: Some(source.into()) }
}

/// The tools the lines of a tool list name (`- [source] name: ...`), and nothing else.
fn listed_tools(block: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in block.lines() {
        let Some((_, tail)) = line.strip_prefix("- [").and_then(|after| after.split_once("] ")) else { continue };
        let Some((name, _)) = tail.split_once(": ") else { continue };
        // A rule that happens to start the same way ("- [host] tools list their arguments first...") has words in it; a tool name has none.
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    names
}

/// Every tool a coding prompt names: the examples (`{"tool":"NAME"...`, but not the placeholder `NAME`), the tool list, and any word of its sentences that is written like a tool
/// (`pack.name` for a pack the router has, or the loop's own `todo.` and `agent.`): "to create a whole file use fs.write" names `fs.write`.
fn tools_named_in(block: &str) -> Vec<String> {
    let mut names = listed_tools(block);
    let mut rest = block;
    while let Some(at) = rest.find("\"tool\":\"") {
        rest = &rest[at + "\"tool\":\"".len()..];
        let name = &rest[..rest.find('"').unwrap_or(rest.len())];
        if name != "NAME" && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    let prefixes: Vec<&str> = crate::tool_providers::packs().iter().flat_map(|p| p.prefixes.iter().copied()).chain(["todo.", "agent."]).collect();
    for word in block.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_'))) {
        let word = word.trim_matches('.');
        let written_like_a_tool = prefixes.iter().any(|p| word.strip_prefix(p).is_some_and(|tail| !tail.is_empty() && tail.chars().all(|c| c.is_ascii_lowercase() || c == '_')));
        if written_like_a_tool && !names.iter().any(|n| n == word) {
            names.push(word.to_string());
        }
    }
    names
}

/// The names the reply may use, from the grammar the loop gives the model (`Mode::Names`, the default).
fn enum_of(shown: &[McpTool]) -> Vec<String> {
    let schema = action_schema_with(shown, Mode::Names, 1, Extras::default());
    schema["$defs"]["call"]["properties"]["tool"]["enum"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// What the loop shows for a goal: the host's tools by the router, then the loop's own (`collect_tools_shared` does the same), in the order the prompt lists them.
fn shown_for(host: &aqua_tool_host::ToolHost, goal: &str) -> Vec<McpTool> {
    let mut shown = crate::agent_tools::tools_for_goal(host, goal);
    let own = agent_tools_for(goal, &shown);
    shown.extend(own);
    in_prompt_order(shown)
}

fn switches(agent_tools: bool, packs: bool) -> [(&'static str, Option<&'static str>); 2] {
    [("AQUA_AGENT_TOOLS", Some(if agent_tools { "on" } else { "off" })), ("AQUA_CODING_PACKS", Some(if packs { "on" } else { "off" }))]
}

#[test]
fn company_tool_adds_a_graph_grounding_rule_only_when_offered() {
    let company = mk("company.search_context", "host");
    let with = planner_prompt_block_with("who owns the project?", &[company], true);
    assert!(with.contains("call company.search_context before making any company-specific claim"), "{with}");
    assert!(with.contains("cite the numbered sources") && with.contains("do not guess"), "{with}");
    assert!(!with.contains("company.remember_exchange"), "a prompt must not name an unoffered tool: {with}");
    let with_memory = planner_prompt_block_with("who owns the project?", &[mk("company.search_context", "host"), mk("company.remember_exchange", "host")], true);
    assert!(with_memory.contains("call company.remember_exchange"), "{with_memory}");
    let without = planner_prompt_block_with("open notepad", &[mk("App", "windows")], true);
    assert!(!without.contains("Company answers:"), "{without}");
}

const CODING_GOALS: &[&str] = &[
    "fix the failing test in rust_probe",
    "commit the fix in the repo and run cargo test",
    "rename util.rs to helpers.rs and fix the imports, then run cargo build",
    "delete the unused file src/old.rs and run cargo build",
    "fix the compose file in the project after checking the docker logs, then run cargo test",
    "find where parse_line is used, fix the bug in it and run cargo test",
];

const OTHER_GOALS: &[&str] =
    &["implement caching in utils.py", "read the file notes.txt in my folder", "show the git branches in the repo", "what's the weather in Delhi", "open notepad and type hello"];

/// The scanner itself: it finds the example a prompt names and skips the placeholder, so that the tests below can fail.
#[test]
fn the_scanner_finds_what_a_prompt_names() {
    let block = "Rules:\n- Use {\"tool\":\"agent.explore\",\"arguments\":{}} to search.\n- To create a whole file use fs.write. A file called lib.rs is not a tool.\n- Reply with ONLY JSON: {\"tool\":\"NAME\",\"arguments\":{...}}\n\nTools (name - description):\n- [host] fs.read: (path) reads\n- [agent] todo.write: (items) keeps\n- [host] tools list their arguments first, e.g. fs.read (path): pass them\n";
    let mut found = tools_named_in(block);
    found.sort();
    assert_eq!(found, vec!["agent.explore", "fs.read", "fs.write", "todo.write"]);
    assert_eq!(listed_tools(block), vec!["fs.read", "todo.write"]);
    assert!(enum_of(&[mk("fs.read", "host"), mk("todo.write", "agent")]).contains(&"todo.write".to_string()));
    assert!(!enum_of(&[mk("fs.read", "host")]).contains(&"agent.explore".to_string()), "the grammar names only what is shown");
}

/// Card OA5-03 (CX-17): the coding prompt tells how to use the outline only when the tool is on offer, like every other tool it describes.
#[test]
fn the_outline_is_described_only_when_it_is_on_offer() {
    let goal = "fix the failing test in rust_probe";
    let prompt = |tools: &[(&str, &str)]| planner_prompt_block_with(goal, &in_prompt_order(tools.iter().map(|(n, s)| mk(n, s)).collect()), true);
    let base = [("fs.read", "host"), ("fs.edit", "host"), ("term.run", "host")];
    let plain = prompt(&base);
    assert!(plain.starts_with("You are Aqua, a coding agent") && !plain.contains("fs.outline") && !plain.contains("outline"), "{plain}");
    let mut with = base.to_vec();
    with.push(("fs.outline", "host"));
    let on = prompt(&with);
    assert!(on.contains("\"tool\":\"fs.outline\"") && on.contains("Then read just that part with fs.read and offset_line"), "{on}");
    // The line names a tool that is in the list that follows it (the tool is on offer), once as the example and once in the list.
    assert!(on.matches("fs.outline").count() >= 2, "{on}");
}

/// Card OA6-02 (ED-07): the goals of the edit-format bake-off (`scripts/eval/edit_bakeoff`, which writes them: a verb, the file, the folder of the task) are requests to work on code, so each run
/// of the bake-off is a coding run with the coding prompt, the read-first rule and the tools a coding run is shown. (The same shape, three of its goals, as the harness writes them.)
#[test]
fn the_goals_of_the_edit_bakeoff_are_coding_goals() {
    for goal in [
        "Edit src/lib.rs in the folder s01_rust_sum of my workspace. The function `add` subtracts. Make its body the single line `a + b` (indented with 4 spaces). Leave `mul` and the rest as it is. Change the file with the fs.edit tool.",
        "Edit NOTES.md in the folder d04_md_section of my workspace. Delete the section `## Deprecated`: the heading line, the empty line after it, the sentence, and the empty line after the sentence. Change the file with the fs.edit_lines tool.",
        "Edit config.json in the folder c03_json_crlf of my workspace. Add the line `\"private\": true,` (two spaces of indentation) right after the `\"version\"` line. The file has CRLF line endings and must keep them.",
    ] {
        assert!(crate::modules::run_profile::routed_as_work(goal), "{goal}");
    }
}

/// Card OA6-02 (ED-07): the coding prompt teaches `fs.edit_lines` only when the tool is on offer, like every other tool it describes. With it not there the prompt is the one it was: the same
/// text as for a list of tools that never had it, and no word about lines or anchors.
#[test]
fn the_line_edit_is_described_only_when_it_is_on_offer() {
    let goal = "fix the failing test in rust_probe";
    let prompt = |tools: &[(&str, &str)]| planner_prompt_block_with(goal, &in_prompt_order(tools.iter().map(|(n, s)| mk(n, s)).collect()), true);
    let base = [("fs.read", "host"), ("fs.edit", "host"), ("fs.patch", "host"), ("term.run", "host")];
    let plain = prompt(&base);
    assert!(plain.starts_with("You are Aqua, a coding agent") && !plain.contains("fs.edit_lines") && !plain.contains("expect_first_line") && !plain.contains("anchor"), "{plain}");
    let mut with = base.to_vec();
    with.push(("fs.edit_lines", "host"));
    let on = prompt(&with);
    assert!(on.contains("\"tool\":\"fs.edit_lines\"") && on.contains("expect_first_line") && on.contains("nothing is changed and the real lines are shown"), "{on}");
    // One line is added for it, where the other tools' lines are, and once in the list; what comes before and after is the plain prompt.
    assert_eq!(on.matches("\"tool\":\"fs.edit_lines\"").count(), 1, "one example: {on}");
    assert_eq!(listed_tools(&on).iter().filter(|n| *n == "fs.edit_lines").count(), 1, "and one entry in the list: {on}");
    let without_line: String = on.lines().filter(|l| !l.contains("fs.edit_lines")).collect::<Vec<_>>().join("\n");
    let plain_lines: String = plain.lines().collect::<Vec<_>>().join("\n");
    assert_eq!(without_line, plain_lines, "nothing else in the prompt changed");
    // The reply may name it when it is shown, and may not when it is not.
    let shown: Vec<McpTool> = with.iter().map(|(n, s)| mk(n, s)).collect();
    assert!(enum_of(&shown).contains(&"fs.edit_lines".to_string()));
    assert!(!enum_of(&base.iter().map(|(n, s)| mk(n, s)).collect::<Vec<_>>()).contains(&"fs.edit_lines".to_string()));
    // The two edit tools are both named in a prompt that has both, and the first rule still says how to change a file with fs.edit.
    assert!(on.contains("Change a file with {\"tool\":\"fs.edit\""), "{on}");
}

/// Card OA12-02: the coding prompt teaches `code.symbols` and `code.outline` only when the tools are on offer; with `code.outline` there, its line stands in place of the one for `fs.outline`.
#[test]
fn the_code_tools_are_described_only_when_they_are_on_offer() {
    let goal = "fix the failing test in rust_probe";
    let prompt = |tools: &[(&str, &str)]| planner_prompt_block_with(goal, &in_prompt_order(tools.iter().map(|(n, s)| mk(n, s)).collect()), true);
    let base = [("fs.read", "host"), ("fs.edit", "host"), ("term.run", "host")];
    let plain = prompt(&base);
    assert!(plain.starts_with("You are Aqua, a coding agent") && !plain.contains("code.symbols") && !plain.contains("code.outline"), "{plain}");
    let mut with = base.to_vec();
    with.extend([("code.symbols", "host"), ("code.outline", "host")]);
    let on = prompt(&with);
    assert!(on.contains("\"tool\":\"code.symbols\",\"arguments\":{\"query\":\"name\"}") && on.contains("\"tool\":\"code.outline\""), "{on}");
    assert!(on.matches("code.symbols").count() >= 2 && on.matches("code.outline").count() >= 2, "once as the example and once in the list: {on}");
    // Only the tool that is on offer is described.
    let mut only_symbols = base.to_vec();
    only_symbols.push(("code.symbols", "host"));
    let s = prompt(&only_symbols);
    assert!(s.contains("\"tool\":\"code.symbols\"") && !s.contains("code.outline"), "{s}");
    // With both outlines on offer the prompt teaches the one with the first lines and not the other: one line less.
    let mut both = with.clone();
    both.push(("fs.outline", "host"));
    let b = prompt(&both);
    assert!(b.contains("\"tool\":\"code.outline\"") && !b.contains("\"tool\":\"fs.outline\""), "{b}");
    assert!(b.lines().filter(|l| l.contains("fs.outline")).count() == 1, "it is still in the list of tools, on one line, and nowhere else: {b}");
    // With only fs.outline the prompt is what it was.
    let mut old = base.to_vec();
    old.push(("fs.outline", "host"));
    assert!(prompt(&old).contains("Then read just that part with fs.read and offset_line"));
}

/// Card OA12-03: the coding prompt teaches `code.deps`, `code.refs` and `code.map` only when each is on offer, one line each, and says nothing of them otherwise.
#[test]
fn the_graph_tools_are_described_only_when_they_are_on_offer() {
    let goal = "fix the failing test in rust_probe";
    let prompt = |tools: &[(&str, &str)]| planner_prompt_block_with(goal, &in_prompt_order(tools.iter().map(|(n, s)| mk(n, s)).collect()), true);
    let base = [("fs.read", "host"), ("fs.edit", "host"), ("term.run", "host"), ("code.symbols", "host"), ("code.outline", "host")];
    let plain = prompt(&base);
    for name in ["code.deps", "code.refs", "code.map"] {
        assert!(!plain.contains(name), "{name} is not on offer: {plain}");
    }
    for (name, example) in [
        ("code.deps", "\"tool\":\"code.deps\",\"arguments\":{\"path\":"),
        ("code.refs", "\"tool\":\"code.refs\",\"arguments\":{\"symbol\":\"name\"}"),
        ("code.map", "\"tool\":\"code.map\",\"arguments\":{\"goal\":"),
    ] {
        let mut with = base.to_vec();
        with.push((name, "host"));
        let on = prompt(&with);
        assert!(on.contains(example), "{name}: {on}");
        assert_eq!(on.lines().count(), plain.lines().count() + 2, "{name}: one line to teach it and one in the list of tools");
        for other in ["code.deps", "code.refs", "code.map"].into_iter().filter(|o| *o != name) {
            assert!(!on.contains(other), "only the tool that is on offer is described: {other} in {on}");
        }
    }
}

/// Card OA12-04: the coding prompt teaches the language-server tools in one line, only when they are on offer, and names each of the four only when it is; with none on offer the prompt is what it was.
#[test]
fn the_language_server_tools_are_described_only_when_they_are_on_offer() {
    let goal = "fix the failing test in rust_probe";
    let prompt = |tools: &[(&str, &str)]| planner_prompt_block_with(goal, &in_prompt_order(tools.iter().map(|(n, s)| mk(n, s)).collect()), true);
    let base = [("fs.read", "host"), ("fs.edit", "host"), ("term.run", "host"), ("code.symbols", "host"), ("code.outline", "host")];
    let plain = prompt(&base);
    let four = ["code.definition", "code.references", "code.hover", "code.document_symbols"];
    for name in four {
        assert!(!plain.contains(name), "{name} is not on offer: {plain}");
    }
    assert!(!plain.contains("language server"), "{plain}");
    let mut all = base.to_vec();
    all.extend(four.iter().map(|n| (*n, "host")));
    let on = prompt(&all);
    assert!(on.contains("\"tool\":\"code.definition\",\"arguments\":{\"path\":"), "{on}");
    assert_eq!(on.lines().count(), plain.lines().count() + 1 + 4, "one line to teach them and one for each in the list of tools");
    for name in four {
        assert!(on.matches(name).count() >= 1, "{name}: {on}");
    }
    assert!(on.contains("The first call of a run may ask the person to start the server."), "{on}");
    // Only what is on offer is named.
    let mut one = base.to_vec();
    one.push(("code.definition", "host"));
    let o = prompt(&one);
    assert!(o.contains("\"tool\":\"code.definition\"") && !o.contains("code.references") && !o.contains("code.hover") && !o.contains("code.document_symbols"), "{o}");
    // The reply can name the tools that are shown and not the others.
    let shown: Vec<McpTool> = all.iter().map(|(n, s)| mk(n, s)).collect();
    for name in four {
        assert!(enum_of(&shown).contains(&name.to_string()));
    }
    assert!(!enum_of(&base.iter().map(|(n, s)| mk(n, s)).collect::<Vec<_>>()).contains(&"code.definition".to_string()));
}

/// Card OA4-05: the coding prompt teaches `check.run` and `project.profile` only when the tools are on offer, and with them the rule about running tests and builds is still there.
#[test]
fn the_check_tools_are_described_only_when_they_are_on_offer() {
    let goal = "fix the failing test in rust_probe";
    let prompt = |tools: &[(&str, &str)]| planner_prompt_block_with(goal, &in_prompt_order(tools.iter().map(|(n, s)| mk(n, s)).collect()), true);
    let base = [("fs.read", "host"), ("fs.edit", "host"), ("term.run", "host")];
    let plain = prompt(&base);
    assert!(plain.starts_with("You are Aqua, a coding agent") && !plain.contains("check.run") && !plain.contains("project.profile"), "{plain}");
    let mut with = base.to_vec();
    with.extend([("check.run", "host"), ("project.profile", "host")]);
    let on = prompt(&with);
    assert!(on.contains("\"tool\":\"check.run\",\"arguments\":{\"kind\":\"tests\"}") && on.contains("\"tool\":\"project.profile\""), "{on}");
    assert!(on.contains("Use it in place of term.run for these."), "{on}");
    // The rule that a fix is claimed only after a run that exited 0 is still there; the tools are in the list that follows the rules.
    assert!(on.contains("Do not say it is fixed until a test or build run AFTER your last change exited 0."), "{on}");
    assert!(on.matches("check.run").count() >= 2 && on.matches("project.profile").count() >= 2, "{on}");
}

/// AG-01: for every goal, with the loop's own tools and the extra packs on and off, every tool the prompt names (the examples and the list) is in the enum the reply is held to.
#[test]
fn every_tool_named_in_a_prompt_block_is_in_the_reply_enum() {
    let h = crate::agent_tools::tests::wide_host();
    for agent_tools in [false, true] {
        for packs in [false, true] {
            crate::flags::with_overrides(&switches(agent_tools, packs), || {
                for goal in CODING_GOALS.iter().chain(OTHER_GOALS) {
                    let shown = shown_for(&h, goal);
                    let allowed = enum_of(&shown);
                    let block = planner_prompt_block_with(goal, &shown, true);
                    let coding_prompt = block.starts_with("You are Aqua, a coding agent");
                    // The coding prompt is the one with examples of its own; the general one gives `term.run` and `browser_click` as examples "when listed", so only its list is held to the enum.
                    let named = if coding_prompt { tools_named_in(&block) } else { listed_tools(&block) };
                    for name in &named {
                        assert!(allowed.contains(name), "{goal} (loop tools {agent_tools}, packs {packs}): the prompt names {name}, which the reply cannot: {allowed:?}\n{block}");
                    }
                    if CODING_GOALS.contains(goal) {
                        assert!(coding_prompt, "{goal}: the coding prompt is the one used");
                    }
                }
            });
        }
    }
}

/// Card OA11-04: `agent.task` is on offer to a coding run only with `AQUA_AGENTS=on`, the prompt describes it only then, the reply can name it, and the helpers there are are the values of its
/// `agent` argument. A goal that is not a coding goal is never offered it.
#[test]
fn the_coding_prompt_names_agent_task_only_when_it_is_on_offer_and_the_reply_can_name_it() {
    let h = Arc::new(crate::agent_tools::tests::wide_host());
    let world = crate::test_support::TempWorld::new("agent_task_prompt");
    crate::runtime::data_tripwire::with_workspace_root(&world.root, || {
        crate::agent_tools::with_host(h.clone(), || {
            for goal in CODING_GOALS {
                crate::flags::with_overrides(&[("AQUA_AGENTS", None)], || {
                    let shown = shown_for(&h, goal);
                    let block = planner_prompt_block_with(goal, &shown, true);
                    assert!(
                        !shown.iter().any(|t| t.name == "agent.task") && !block.contains("agent.task"),
                        "{goal}: with agents off the tool is not on offer and the prompt does not name it:\n{block}"
                    );
                });
                crate::flags::with_overrides(&[("AQUA_AGENTS", Some("on"))], || {
                    let shown = shown_for(&h, goal);
                    let task = shown.iter().find(|t| t.name == "agent.task").unwrap_or_else(|| panic!("{goal}: a coding goal is offered the helper with agents on"));
                    assert_eq!(task.source.as_deref(), Some(AGENT_SOURCE));
                    assert!(task.description.chars().count() <= 80, "a tool that is not the host's is shown by its first 80 characters: {}", task.description);
                    let helpers: Vec<String> = task.input_schema["properties"]["agent"]["enum"].as_array().unwrap().iter().filter_map(|v| v.as_str().map(String::from)).collect();
                    assert_eq!(helpers, ["explore", "review"], "the agents that may be helpers");
                    assert!(enum_of(&shown).contains(&"agent.task".to_string()), "the reply can name it");
                    let block = planner_prompt_block_with(goal, &shown, true);
                    assert!(
                        block.contains("{\"tool\":\"agent.task\"") && block.contains("a helper searches and reads in a context of its own"),
                        "{goal}: the coding prompt tells when to use it:\n{block}"
                    );
                    for name in tools_named_in(&block) {
                        assert!(enum_of(&shown).contains(&name), "{goal}: the prompt names {name}, which the reply cannot");
                    }
                });
            }
            // Not a coding goal: never offered, even with agents on. A helper is not offered to itself either.
            crate::flags::with_overrides(&[("AQUA_AGENTS", Some("on"))], || {
                for goal in OTHER_GOALS.iter().filter(|g| !crate::modules::run_profile::coding(g)) {
                    assert!(!shown_for(&h, goal).iter().any(|t| t.name == "agent.task"), "{goal}");
                }
                let _inside = crate::agents::enter_helper().unwrap();
                assert!(!shown_for(&h, CODING_GOALS[0]).iter().any(|t| t.name == "agent.task"), "a helper cannot start a helper, so it is not shown the tool");
            });
        });
    });
}

/// TL-02: the same, from the other side. The coding prompt describes a tool only when the tool is on offer, and offers the loop's own only when they are switched on.
#[test]
fn coding_prompt_names_only_grammar_tools() {
    let goal = "fix the failing test in rust_probe";
    let prompt = |tools: &[(&str, &str)]| planner_prompt_block_with(goal, &in_prompt_order(tools.iter().map(|(n, s)| mk(n, s)).collect()), true);
    let base = [("fs.read", "host"), ("fs.edit", "host"), ("term.run", "host")];
    let with = |extra: &[(&'static str, &'static str)]| -> String {
        let mut all = base.to_vec();
        all.extend_from_slice(extra);
        prompt(&all)
    };
    let plain = with(&[]);
    assert!(plain.starts_with("You are Aqua, a coding agent"));
    for tool in ["fs.grep", "agent.explore", "fs.patch", "git.commit", "term.start", "term.output", "todo.write"] {
        assert!(!plain.contains(tool), "{tool} is described although it is not on offer:\n{plain}");
    }
    // The sentence about `fs.write` is for a run that is shown it.
    assert!(!plain.contains("fs.write") && with(&[("fs.write", "host")]).contains("To create or replace a whole file use fs.write."));
    // `agent.explore` used to be described whenever `fs.grep` was on offer, and no list offered it.
    let grep = with(&[("fs.grep", "host")]);
    assert!(grep.contains("\"tool\":\"fs.grep\"") && !grep.contains("agent.explore"), "{grep}");
    let explore = with(&[("fs.grep", "host"), ("agent.explore", "agent")]);
    assert!(explore.starts_with("You are Aqua, a coding agent"), "the loop's own tools do not make a desktop run of a coding run");
    assert!(explore.contains("\"tool\":\"agent.explore\"") && explore.contains("- [agent] agent.explore: "), "{explore}");
    let todo = with(&[("todo.write", "agent")]);
    assert!(todo.contains("\"tool\":\"todo.write\"") && todo.contains("- [agent] todo.write: ") && !todo.contains("agent.explore"), "{todo}");
    // A line for each of the others appears with the tool, and `term.start` needs `term.output` to be described.
    assert!(with(&[("git.commit", "host")]).contains("\"tool\":\"git.commit\""));
    assert!(!with(&[("term.start", "host")]).contains("term.output"));
    assert!(!with(&[("term.output", "host")]).contains("term.start"));
    let jobs = with(&[("term.start", "host"), ("term.output", "host")]);
    assert!(jobs.contains("term.start (the arguments of term.run)") && jobs.contains("\"tool\":\"term.output\""), "{jobs}");
    assert!(with(&[("fs.patch", "host")]).contains("\"tool\":\"fs.patch\""));
    // Everything it names is in the list it shows, whichever of them are on offer.
    let all = with(&[("fs.grep", "host"), ("fs.patch", "host"), ("git.commit", "host"), ("term.start", "host"), ("term.output", "host"), ("agent.explore", "agent"), ("todo.write", "agent")]);
    let listed = listed_tools(&all);
    for name in tools_named_in(&all) {
        assert!(listed.contains(&name), "{name} is named by the prompt and not in its list: {listed:?}");
    }
}

/// TL-02: in `Mode::Names` the reply may name exactly the tools that were shown, so a tool is reachable if and only if it is in the list the prompt prints.
#[test]
fn no_tool_name_in_prompt_is_missing_from_the_enum() {
    let h = crate::agent_tools::tests::wide_host();
    for agent_tools in [false, true] {
        crate::flags::with_overrides(&switches(agent_tools, false), || {
            for goal in CODING_GOALS.iter().chain(OTHER_GOALS) {
                let shown = shown_for(&h, goal);
                let mut allowed = enum_of(&shown);
                let mut listed = listed_tools(&planner_prompt_block_with(goal, &shown, true));
                allowed.sort();
                listed.sort();
                assert_eq!(listed, allowed, "{goal} (loop tools {agent_tools}): the list the model reads and the names it may write are the same set");
            }
        });
    }
}

/// AG-01: `todo.write` for a coding goal (and a goal that wants a plan), `agent.explore` when the host's `fs.grep` is shown, nothing for a weather question or with the switch off.
#[test]
fn todo_write_is_offered_for_a_coding_goal_and_not_for_a_weather_question() {
    let with_grep = [mk("fs.read", "host"), mk("fs.grep", "host")];
    let names = |goal: &str, shown: &[McpTool]| agent_tools_for(goal, shown).into_iter().map(|t| t.name).collect::<Vec<_>>();
    crate::flags::with_overrides(&[("AQUA_AGENT_TOOLS", Some("on")), ("AQUA_PLAN", None)], || {
        assert_eq!(names("fix the failing test in rust_probe", &with_grep), ["todo.write", "agent.explore"]);
        assert!(names("what is the weather in Delhi", &with_grep).is_empty());
        assert!(names("open notepad and type hello", &[]).is_empty());
        assert_eq!(names("fix the failing test in rust_probe", &[mk("fs.read", "host")]), ["todo.write"], "no search to explore with");
        // A tool that only has the name `fs.grep` (a desktop or browser tool) is not the host's search.
        assert_eq!(names("fix the failing test in rust_probe", &[mk("fs.grep", "windows")]), ["todo.write"]);
        // A goal that is not coding wants no checklist unless it wants a plan.
        assert!(names("read the files a.txt and b.txt and tell me what each says", &with_grep).is_empty());
    });
    crate::flags::with_overrides(&[("AQUA_AGENT_TOOLS", Some("on")), ("AQUA_PLAN", Some("on"))], || {
        assert_eq!(names("read the files a.txt and b.txt and tell me what each says", &with_grep), ["todo.write"], "a goal that wants a plan can keep a checklist");
        assert!(names("what is the weather in Delhi", &with_grep).is_empty());
    });
    for off in [None, Some("off"), Some("0"), Some("")] {
        crate::flags::with_overrides(&[("AQUA_AGENT_TOOLS", off)], || assert!(names("fix the failing test in rust_probe", &with_grep).is_empty(), "AQUA_AGENT_TOOLS = {off:?}"));
    }
    // What the model reads about them fits its line (a tool that is not the host's is cut at 80 characters), and what the loop answers to is the schema it was given.
    crate::flags::with_overrides(&[("AQUA_AGENT_TOOLS", Some("on"))], || {
        for tool in agent_tools_for("fix the failing test in rust_probe", &with_grep) {
            assert_eq!(tool.source.as_deref(), Some(AGENT_SOURCE));
            assert!(tool.description.chars().count() <= 80, "{}: {}", tool.name, tool.description);
            assert!(tool.description.starts_with('('), "{}: the arguments come first, like the host's tools", tool.name);
        }
    });
}

/// The switch reaches the loop's tool list: `collect_tools_shared` adds the loop's tools to the host's, and not to a run that is shown no host tool.
#[test]
fn the_loops_tool_list_gains_the_two_tools_with_the_switch_on() {
    let _host = super::tests::shared_host();
    let names = |goal: &str| collect_tools_shared(None, None, goal).unwrap().into_iter().map(|t| t.name).collect::<Vec<_>>();
    crate::flags::with_overrides(&[("AQUA_AGENT_TOOLS", Some("on"))], || {
        let coding = names("fix the failing test in a.txt");
        assert!(coding.contains(&"todo.write".to_string()) && coding.contains(&"fs.read_file".to_string()), "{coding:?}");
        assert!(!coding.contains(&"agent.explore".to_string()), "the test host has no fs.grep to explore with: {coding:?}");
        let desktop = names("open notepad");
        assert!(!desktop.contains(&"todo.write".to_string()), "{desktop:?}");
    });
    crate::flags::with_overrides(&[("AQUA_AGENT_TOOLS", None)], || {
        assert!(!names("fix the failing test in a.txt").contains(&"todo.write".to_string()));
    });
}

/// AG-01 (checklist): the list is recited at the end of the prompt, cut to fit, with what did not fit counted.
#[test]
fn the_checklist_goes_at_the_end_of_the_prompt_cut_to_fit() {
    reset_checklist();
    assert_eq!(checklist_tail(), "");
    replace_checklist(&json!({"items": [{"text": "read a.rs", "status": "doing"}, {"text": "fix it", "status": "pending"}]}));
    assert_eq!(checklist_tail(), " Your checklist (keep it current with todo.write; one item is doing): 1. [doing] read a.rs; 2. [pending] fix it.");
    let many: Vec<Value> = (0..20).map(|i| json!({"text": format!("step number {i} {}", "x".repeat(70)), "status": "pending"})).collect();
    replace_checklist(&json!({"items": many}));
    let tail = checklist_tail();
    assert!(tail.chars().count() < CHECKLIST_TAIL_CHARS + 160, "{} characters", tail.chars().count());
    assert!(tail.contains("(and ") && tail.contains(" more)"), "{tail}");
    assert!(tail.contains("1. [pending] step number 0 "), "{tail}");
    reset_checklist();
    assert_eq!(checklist_tail(), "");
}

/// Card OA4-05 (CU-04, D-CU-3): the brief describes the folder the goal names, climbed to the project that holds it, and not the first root.
#[test]
fn the_brief_describes_the_folder_the_goal_names_not_the_first_root() {
    let d = std::env::temp_dir().join(format!("aqua_cu04_brief_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("ws/app/src")).unwrap();
    std::fs::create_dir_all(d.join("ws/other")).unwrap();
    std::fs::write(d.join("ws/Cargo.toml"), "[package]\nname = \"top\"\n").unwrap();
    std::fs::write(d.join("ws/app/pyproject.toml"), "[project]\nname = \"app\"\n").unwrap();
    std::fs::write(d.join("ws/app/src/x.py"), "x = 1\n").unwrap();
    let roots = vec![d.join("ws")];
    let real = |p: std::path::PathBuf| std::fs::canonicalize(p).unwrap();
    assert_eq!(goal_project_dir_in("fix the bug in app/src/x.py", &roots), Some(real(d.join("ws/app"))), "a file is in the project that holds it");
    assert_eq!(goal_project_dir_in("fix the bug in the app folder, app/src", &roots), Some(real(d.join("ws/app"))));
    assert_eq!(goal_project_dir_in("fix the failing test", &roots), Some(real(d.join("ws"))), "a goal that names none is about the first root, as it was");
    assert_eq!(goal_project_dir_in("tidy other", &roots), Some(real(d.join("ws"))), "a folder with no project file is looked for upward, up to the root that holds it");
    let named = crate::modules::project_brief::brief(&goal_project_dir_in("fix the bug in app/src/x.py", &roots).unwrap());
    assert!(named.contains("Python") && !named.contains("Rust package: top"), "{named}");
    let first = crate::modules::project_brief::brief(&goal_project_dir_in("fix the failing test", &roots).unwrap());
    assert!(first.contains("Rust package: top") && !first.contains("Python"), "{first}");
    let _ = std::fs::remove_dir_all(&d);
}

/// Card OA4-05 (CU-04): the check after an edit runs in the project of the edited file, with the file by its whole path, and a file outside the folders the tools may use has no project.
#[test]
fn the_check_for_an_edit_runs_in_the_project_of_the_edited_file() {
    use aqua_native_tools::fs::{FsConfig, PathGuard};
    let d = std::env::temp_dir().join(format!("aqua_cu04_check_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("ws/app")).unwrap();
    std::fs::create_dir_all(d.join("ws/tools")).unwrap();
    std::fs::create_dir_all(d.join("outside")).unwrap();
    std::fs::write(d.join("ws/Cargo.toml"), "[package]\nname = \"top\"\n").unwrap();
    std::fs::write(d.join("ws/app/pyproject.toml"), "[project]\nname = \"app\"\n").unwrap();
    std::fs::write(d.join("ws/app/main.py"), "x = 1\n").unwrap();
    std::fs::write(d.join("ws/tools/run.py"), "y = 2\n").unwrap();
    std::fs::write(d.join("outside/o.py"), "z = 3\n").unwrap();
    let guard = PathGuard::new(FsConfig::with_workspace(d.join("ws").to_string_lossy().to_string()), vec![]).unwrap();
    let real = |p: std::path::PathBuf| std::fs::canonicalize(p).unwrap();
    let (dir, abs) = project_of_edit(&guard, "app/main.py").expect("the file is in a project");
    assert_eq!(real(dir.clone()), real(d.join("ws/app")));
    assert_eq!(real(abs.clone()), real(d.join("ws/app/main.py")));
    assert_eq!(real(project_of_edit(&guard, "tools/run.py").unwrap().0), real(d.join("ws")), "a file with no project of its own is in the project above it");
    assert!(project_of_edit(&guard, &d.join("outside/o.py").to_string_lossy()).is_none(), "outside the folders the tools may use");
    // The request: the check of a Python file is `python -m py_compile <the file>`, started in the project.
    let args = crate::modules::coding_runner::host_check_request("fs.edit", &abs.to_string_lossy(), "Edited app/main.py", &dir).expect("a Python file has a check that is on by default");
    assert_eq!(args["cwd"].as_str().map(|c| real(std::path::PathBuf::from(c))), Some(real(d.join("ws/app"))));
    assert_eq!(args["args"][2].as_str().map(|f| real(std::path::PathBuf::from(f))), Some(real(d.join("ws/app/main.py"))));
    let _ = std::fs::remove_dir_all(&d);
}

//! Which packs of tools a goal calls for: the words of a goal, the router of the second generation and the order of the packs (move only: this was part of `agent_tools.rs`).

use super::*;

// The routing packs (one per family of tools: its name, the prefix of its tools and the words of a goal that bring them to the agent) are defined in the rows of
// `crate::tool_providers::PROVIDERS`, in the order the list had when it was kept here; `crate::tool_providers::packs()` is that list.
// The clipboard tools exist only when `AQUA_CLIPBOARD_TOOLS=on` registered them (the `clipboard` row), and `research.run` only when `AQUA_RESEARCH=on` did (the `research` row): with a switch
// off, the pack of that family finds nothing to show, and the research pack takes part in the routing only then (`route_by_words_with`).

pub(super) fn words_of(goal: &str) -> (String, HashSet<String>) {
    let lower = goal.to_lowercase();
    let set = lower.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == '-')).map(|w| w.trim_matches('.').to_string()).filter(|w| !w.is_empty()).collect();
    (lower, set)
}

/// The pack a tool belongs to: the one whose prefix its name starts with (a test checks that every tool the host registers has one, or another way to the agent).
#[cfg(test)]
pub(crate) fn pack_of(tool_name: &str) -> Option<&'static str> {
    crate::tool_providers::packs().iter().find(|p| p.prefixes.iter().any(|pre| tool_name.starts_with(pre))).map(|p| p.name)
}

/// Is the second generation of the router on? `AQUA_ROUTER_V2=0` brings back the earlier rules and tool order, to compare
/// the two or to roll back.
pub fn router_v2() -> bool {
    crate::flags::is_on(&crate::flags::ROUTER_V2)
}

/// Is this goal one the router shows the coding set (files and terminal, git when a repository is named)? The second generation of the router has to be on.
pub fn is_coding_goal(goal: &str) -> bool {
    router_v2() && crate::modules::run_profile::routed_as_work(goal)
}

/// Is this about the weather or a forecast? The weather has its own service, so it is not sent to a web search.
pub(crate) fn asks_about_weather(goal: &str) -> bool {
    let g = goal.to_ascii_lowercase();
    g.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| matches!(w, "weather" | "forecast" | "temperature" | "rain" | "raining"))
}

/// The packs a goal calls for, best match first. A request to work on code or files (fix, implement, refactor, edit… with a
/// file or a project) gets the files and terminal packs, and git when it names a repository; anything else gets at most
/// [`MAX_PACKS`] by the words it mentions.
pub fn route(goal: &str) -> Vec<&'static str> {
    route_with(goal, router_v2())
}

pub(super) fn route_with(goal: &str, v2: bool) -> Vec<&'static str> {
    let by_words = route_by_words(goal);
    if v2 && crate::modules::run_profile::routed_as_work(goal) {
        let mut packs = vec!["files", "terminal"];
        if by_words.contains(&"git") {
            packs.push("git");
        }
        return packs;
    }
    // A question that has to be looked up, or whose answer is live (news, prices), is answered from what a search and the pages it names say, with
    // each claim cited: the web tools come first.
    // (Weather has a service of its own, asked before the agent; a search engine is the wrong place to look for it.)
    if v2 && (crate::laya_validator::needs_web_lookup(goal) || crate::laya_validator::looks_live_info(goal)) && !asks_about_weather(goal) {
        let mut packs = vec!["web"];
        packs.extend(by_words.into_iter().filter(|p| *p != "web").take(MAX_PACKS - 1));
        return packs;
    }
    by_words
}

fn route_by_words(goal: &str) -> Vec<&'static str> {
    route_by_words_with(goal, crate::research::enabled())
}

/// The packs the words of a goal pick. `research_on`: the research pack takes part only when the switch is on, so that with it off a goal that says "research" is routed as it always was.
pub(super) fn route_by_words_with(goal: &str, research_on: bool) -> Vec<&'static str> {
    scored_packs(goal, research_on).into_iter().take(MAX_PACKS).map(|(_, _, n)| n).collect()
}

/// Every pack the words of a goal pick, best first: (words that matched, where the pack sits in the list, its name).
pub(super) fn scored_packs(goal: &str, research_on: bool) -> Vec<(usize, usize, &'static str)> {
    let (lower, set) = words_of(goal);
    let mut scored: Vec<(usize, usize, &'static str)> = crate::tool_providers::packs()
        .iter()
        .enumerate()
        .filter(|(_, p)| research_on || p.name != "research")
        .filter_map(|(i, p)| {
            let hits = p
                .words
                .iter()
                .filter(|w| if let Some(ext) = w.strip_prefix('.') { lower.split_whitespace().any(|t| t.trim_matches(|c: char| !c.is_alphanumeric() && c != '.').ends_with(&format!(".{ext}"))) } else if w.contains(' ') { lower.contains(**w) } else { set.contains(**w) })
                .count();
            (hits > 0).then_some((hits, i, p.name))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored
}

//! The lists of the skills that narrowed a run, found again from its record, and how they go on to the run that continues it (card OA13-03, task AQ-12: the rest of what was here is `agent_runs::scope`; every name is still reached from `agent_tools` as before). They stay because `skills` is named here (`Skills`, `for_run`, `shipped::lookup`) and `agent_runs` does not name it. `agent_runs::scope` calls `inherit_marks_on` through the one edge that exists, `agent_runs -> agent_tools`.

use super::*;

/// The lists that narrowed a run, found again from the record: the `skill.load` calls of the run and of the runs it went on from (as far as eight back), each skill's list as it is now,
/// and the list of tools a command allowed each of those runs (`allowed-tools`, kept with the run when it was recorded). For a run that goes on after a restart, or from one this process
/// does not remember (card R6-16): the process keeps the newest 64, and a run outside them has only its record.
pub(crate) fn restrictions_from_record(store: &crate::agent_runs::AgentRunStore, skills: &crate::skills::Skills, run: &str) -> Vec<Vec<String>> {
    let mut lists: Vec<Vec<String>> = Vec::new();
    let mut loaded: Vec<String> = Vec::new();
    let mut at = Some(run.to_string());
    for _ in 0..8 {
        let Some(id) = at.take() else { break };
        // (Every list the run was narrowed by when it was recorded: a command's, and, with agents on, the agent's; a call has to be in all of them, card OA11-04.)
        for list in store.allowed_lists_of(&id).unwrap_or_default() {
            if !list.is_empty() && !lists.contains(&list) {
                lists.push(list);
            }
        }
        for step in store.steps(&id).unwrap_or_default() {
            if step["tool"] == "skill.load" && step["status"] == "completed" {
                let args: serde_json::Value = serde_json::from_str(step["arguments"].as_str().unwrap_or("")).unwrap_or_else(|_| step["arguments"].clone());
                if let Some(name) = args["name"].as_str().map(str::to_string) {
                    if !loaded.contains(&name) {
                        // (A skill that ships with Aqua only narrows, so a run that goes on is held to its list even if `AQUA_SHIPPED_SKILLS` was turned off since; the person's own skill of that name is
                        // found first.)
                        let found = skills.get(&name).cloned().or_else(|| crate::skills::shipped::lookup(&name));
                        if let Some(skill) = found.filter(|s| !s.allowed_tools.is_empty()) {
                            lists.push(skill.allowed_tools);
                        }
                        loaded.push(name);
                    }
                }
            }
        }
        at = store.get(&id).ok().flatten().and_then(|r| r["parent_run_id"].as_str().map(str::to_string)).filter(|p| !p.is_empty());
    }
    lists
}

/// `run` goes on from `parent`: it starts as tainted and as private as the parent ended. The parent's marks are those of the last 64 runs this
/// process knows, or else those its record kept (a run from before a restart, or one that stopped in the middle: the record is written as the marks
/// change, so a crash does not wash them off). When nothing is known of the parent and some of its work was carried over (`had_history`), it counts as
/// both, because the safe guess is the one that asks.
pub(crate) fn inherit_marks_on(host: &ToolHost, run: &str, parent: &str, had_history: bool, store: Option<&crate::agent_runs::AgentRunStore>) {
    let remembered = registry().ended_with_lists(parent);
    // The lists of the skills the parent had loaded go on with the run: from what this process remembers, else from the record (a run from before a restart).
    let lists = match &remembered {
        Some((_, _, lists)) => lists.clone(),
        // The skills the new run may use (its own and, when the person trusts it, its project's) are the ones whose lists the parent's `skill.load` calls named.
        None => match store {
            Some(store) => restrictions_from_record(store, &crate::skills::for_run(run), parent),
            None => Vec::new(),
        },
    };
    for list in lists {
        host.restrict_run(run, list);
    }
    let found = remembered.map(|(t, p, _)| (t, p)).or_else(|| store.and_then(|s| s.marks(parent).ok().flatten()));
    let (tainted, private) = found.unwrap_or((had_history, had_history));
    if tainted {
        host.mark_tainted(run);
    }
    if private {
        host.mark_private(run);
    }
}

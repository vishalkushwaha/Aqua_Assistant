//! The coding set: the tools a coding goal is always shown, the room made for the ones its words ask for, and the one more pack that `AQUA_CODING_PACKS` allows (move only: this was part of `agent_tools.rs`).

use super::*;

/// Takes one tool out of a coding run's list to make room, and says whether it did: the last of [`DROPPABLE`] (not one of the background pair while another can go), then, for a tool the goal
/// asked for, the first of `give_way` that is there. Dropping `term.start` or `term.output` drops both, since neither is of use without the other.
fn make_room(out: &mut Vec<McpTool>, give_way: &[&str]) -> bool {
    use crate::modules::coding_session::{BACKGROUND_JOB_PAIR, DROPPABLE};
    let droppable = |t: &McpTool| DROPPABLE.contains(&t.name.as_str());
    let in_pair = |t: &McpTool| BACKGROUND_JOB_PAIR.contains(&t.name.as_str());
    let at = out.iter().rposition(|t| droppable(t) && !in_pair(t)).or_else(|| out.iter().rposition(|t| droppable(t))).or_else(|| give_way.iter().find_map(|n| out.iter().position(|t| t.name == *n)));
    let Some(i) = at else { return false };
    let gone = out.remove(i);
    if BACKGROUND_JOB_PAIR.contains(&gone.name.as_str()) {
        out.retain(|t| !BACKGROUND_JOB_PAIR.contains(&t.name.as_str()));
    }
    true
}

pub(super) fn pin_coding_tools(out: &mut Vec<McpTool>, all: &[ToolManifest], wanted: &[&str]) {
    use crate::modules::coding_session::{BACKGROUND_JOB_PAIR, GIVE_WAY, PINNED_TOOLS, PINNED_WHEN_OFFERED};
    // (A tool of the second list that the host does not list, because its switch is off, is not in `all`, and is skipped.)
    for name in PINNED_TOOLS.iter().chain(PINNED_WHEN_OFFERED) {
        if out.iter().any(|t| t.name == *name) {
            continue;
        }
        let Some(m) = all.iter().find(|m| m.name == *name) else { continue };
        if out.len() >= MAX_HOST_TOOLS && !make_room(out, &[]) {
            break;
        }
        out.push(as_shown(m));
    }
    // What the goal asks for by its words (a rename, a delete, a stage): shown even when the list is full, in place of a droppable tool or, failing that, a read helper.
    for name in wanted {
        if out.iter().any(|t| t.name == *name) {
            continue;
        }
        let Some(m) = all.iter().find(|m| m.name == *name) else { continue };
        if crate::modules::coding_driver::denied(&m.name) {
            continue;
        }
        if out.len() >= MAX_HOST_TOOLS && !make_room(out, GIVE_WAY) {
            continue;
        }
        out.push(as_shown(m));
    }
    // Both of the background pair or neither: the list was cut by places, and a cut can leave one of them.
    let has = |out: &[McpTool], n: &str| out.iter().any(|t| t.name == n);
    for (present, missing) in [(BACKGROUND_JOB_PAIR[0], BACKGROUND_JOB_PAIR[1]), (BACKGROUND_JOB_PAIR[1], BACKGROUND_JOB_PAIR[0])] {
        if has(out, present) && !has(out, missing) {
            match all.iter().find(|m| m.name == missing) {
                Some(m) if out.len() < MAX_HOST_TOOLS => out.push(as_shown(m)),
                _ => out.retain(|t| t.name != present),
            }
        }
    }
}

/// Whether a coding goal may be shown tools of one more pack (`AQUA_CODING_PACKS`).
pub(super) fn coding_packs_on() -> bool {
    crate::flags::is_on(&crate::flags::CODING_PACKS)
}

/// The packs a coding goal may add one more of, when its words name one.
const CODING_EXTRA_PACKS: &[&str] = &["database", "docker", "k8s", "monitor", "forge", "web"];
/// How many tools of that pack it is shown.
const CODING_PACK_TOOLS: usize = 2;
/// The most host tools a coding goal is shown with `AQUA_CODING_PACKS` on: the twelve it is always shown, and two of one more pack.
pub const MAX_CODING_HOST_TOOLS: usize = MAX_HOST_TOOLS + CODING_PACK_TOOLS;

/// Which tools of an extra pack come first for a coding goal, before the goal's own words decide: the ones a developer reaches for.
fn coding_pack_priority(pack: &str) -> &'static [&'static str] {
    match pack {
        "database" => &["db.schema", "db.query"],
        "docker" => &["docker.logs", "docker.ps"],
        "k8s" => &["k8s.logs", "k8s.get"],
        "monitor" => &["mon.log_tail", "mon.health"],
        "forge" => &["forge.issues", "forge.prs", "forge.pr"],
        "web" => &["web.search", "web.read"],
        _ => &[],
    }
}

/// The word without a plural `s`, so that "logs" in a goal finds `mon.log_tail`.
fn stem(word: &str) -> &str {
    if word.len() > 3 { word.strip_suffix('s').unwrap_or(word) } else { word }
}

/// `AQUA_CODING_PACKS` (card OA5-01, WF-11): a coding goal whose words call for one more pack (the database for "write a migration for the users table", docker for "check the docker logs and fix
/// the compose file") is shown [`CODING_PACK_TOOLS`] tools of it, those whose names share the most words with the goal first, then the pack's usual first ones. The pack is the best-scoring of
/// [`CODING_EXTRA_PACKS`]; nothing is added when no word calls for one. The cap becomes [`MAX_CODING_HOST_TOOLS`]. Every tool still goes through the host's decision.
pub(super) fn add_coding_pack(out: &mut Vec<McpTool>, all: &[ToolManifest], goal: &str) {
    let Some(pack) = scored_packs(goal, false).into_iter().map(|(_, _, n)| n).find(|n| CODING_EXTRA_PACKS.contains(n)) else { return };
    let Some(spec) = crate::tool_providers::packs().iter().find(|p| p.name == pack) else { return };
    let (_, set) = words_of(goal);
    let words: HashSet<&str> = set.iter().map(|w| stem(w)).collect();
    let first = coding_pack_priority(pack);
    let rank = |m: &ToolManifest| first.iter().position(|n| *n == m.name).unwrap_or(usize::MAX);
    let hits = |m: &ToolManifest| m.name.split(['.', '_']).filter(|w| words.contains(stem(w))).count();
    let mut mine: Vec<&ToolManifest> = all.iter().filter(|m| spec.prefixes.iter().any(|pre| m.name.starts_with(pre)) && !crate::modules::coding_driver::denied(&m.name) && !out.iter().any(|t| t.name == m.name)).collect();
    mine.sort_by(|a, b| hits(b).cmp(&hits(a)).then(rank(a).cmp(&rank(b))).then(b.annotations.read_only.cmp(&a.annotations.read_only)).then(a.name.cmp(&b.name)));
    out.extend(mine.into_iter().take(CODING_PACK_TOOLS).map(as_shown));
}

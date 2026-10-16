//! The folder of the project a goal is about, and the project notes for the planner (card OA3-05, move only).

use super::*;

/// The folders the agent may work in, as the project's notes are looked for in them (the workspace first).
pub(super) fn project_roots() -> Vec<std::path::PathBuf> {
    match crate::tool_host::project_guard() {
        Some(g) => {
            let mut roots: Vec<(String, std::path::PathBuf)> = g.roots().into_iter().filter(|(_, _, access)| matches!(access, aqua_native_tools::fs::Access::Rw)).map(|(alias, path, _)| (alias, path)).collect();
            roots.sort_by_key(|(alias, _)| alias != "ws");
            roots.into_iter().map(|(_, p)| p).collect()
        }
        None => vec![crate::runtime::workspace_root()],
    }
}

/// The folder of the project a goal is about (card OA4-05, task CU-04: the brief and the check after an edit were aimed at the first root, whatever the goal named): the folder the goal names, or the
/// folder of the file it names, climbed to the nearest folder that holds a project (`Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`, a solution file, `.git` ...) and never above the root that holds
/// it. A goal that names none is about the first root, as it always was.
pub(crate) fn goal_project_dir_in(goal: &str, roots: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    let named = crate::project_instructions::project_dir_for_goal(goal, roots)?;
    let limit = roots.iter().filter_map(|r| std::fs::canonicalize(r).ok()).find(|r| aqua_native_tools::fs::guard::within(&named, r))?;
    Some(aqua_native_tools::code::nearest_root(&named, &limit).unwrap_or(named))
}

/// [`goal_project_dir_in`] for the folders the agent may work in.
pub(super) fn goal_project_dir(goal: &str) -> Option<std::path::PathBuf> {
    goal_project_dir_in(goal, &project_roots())
}

/// The project that holds a file the run changed, and the file as an absolute path (card OA4-05, CU-04): where the check after an edit runs. `None` when the file is outside the folders the tools may
/// use, or no project marker is above it inside the folder that holds it (the check then runs where it always did, in the first root).
pub(crate) fn project_of_edit(guard: &aqua_native_tools::fs::PathGuard, path: &str) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let abs = guard.resolve(path, aqua_native_tools::fs::Op::Read).ok()?.path;
    let limit = guard.root_path_of(&abs)?;
    let dir = aqua_native_tools::code::nearest_root(&abs, &limit)?;
    Some((dir, abs))
}

/// The project's own notes for the planner (see `project_instructions`): the text of a trusted `AQUA.md` or `AGENTS.md`, or a line saying one
/// exists and was not read. Only for a run that is shown the file tools.
pub(super) fn project_notes_for(goal: &str, shown: &[McpTool]) -> String {
    if !shown.iter().any(|t| t.source.as_deref() == Some("host") && t.name.starts_with("fs.")) {
        return String::new();
    }
    // (`AQUA_NESTED_NOTES`, card OA4-05, CU-11: a long note is cut by its sections and not at the 2,048th character.)
    crate::project_instructions::block_for_goal_with(&crate::runtime::data_dir(), goal, &project_roots(), crate::flags::is_on(&crate::flags::NESTED_NOTES)).map(|b| format!("\n{b}")).unwrap_or_default()
}

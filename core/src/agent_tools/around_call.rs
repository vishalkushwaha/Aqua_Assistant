//! What the call to the host does around it: the look at a project before the first command that may change files and at the end of the run, and the question about a folder the person named in their request (move only: this was part of `agent_tools.rs`).

use super::*;

/// Before the first command of a run that may change files (a `term.*` call, or a `git.*` tool that writes the work folder): with `AQUA_SNAPSHOT=on`, and when the project folder of the
/// run is a trusted one, the folder is looked at and the tree is kept (`git::checkpoint`). Later calls of the run do nothing; the second look is made when the run ends (`finish_checkpoint`).
/// A run that cannot get one (the folder is not trusted, it has too many files, the disk is nearly full) goes on as it always did; the reason is in the log once.
pub(super) fn checkpoint_before_command(tool: &str, arguments: &Value, run_id: &str) {
    let on = crate::flags::is_on(&crate::flags::SNAPSHOT);
    if !on {
        return;
    }
    checkpoint_before_command_in(crate::tool_providers::checkpoints().as_deref(), crate::project_context::project_dir_of_run(run_id), tool, arguments, run_id);
}

/// [`checkpoint_before_command`] with the checkpoints and the run's project folder given (the switch has been looked at): what a test drives.
pub(super) fn checkpoint_before_command_in(checkpoints: Option<&aqua_native_tools::git::checkpoint::Checkpoints>, project: Option<std::path::PathBuf>, tool: &str, arguments: &Value, run_id: &str) {
    if !aqua_native_tools::git::checkpoint::tool_may_change_files(tool, arguments) {
        return;
    }
    let Some(checkpoints) = checkpoints else { return };
    // The folder of the run's project; a run whose goal names none works where its commands say (`cwd`), else in the first folder Aqua may use.
    let dir = project
        .or_else(|| arguments.get("cwd").and_then(|v| v.as_str()).filter(|c| !c.is_empty()).and_then(|c| checkpoints.folder_named(c)))
        .or_else(|| checkpoints.default_project());
    let Some(dir) = dir else { return };
    let _ = checkpoints.before_command(run_id, &dir, &std::sync::atomic::AtomicBool::new(false));
}

/// The run is over: when a checkpoint was taken for it, the project folder is looked at again, and the record of the run in the undo store gets the tree at its end (or is taken away when
/// nothing changed). Does nothing for a run that has none.
pub(super) fn finish_checkpoint(run_id: &str) {
    finish_checkpoint_in(crate::tool_providers::checkpoints().as_deref(), run_id);
}

pub(super) fn finish_checkpoint_in(checkpoints: Option<&aqua_native_tools::git::checkpoint::Checkpoints>, run_id: &str) {
    if let Some(changed) = checkpoints.and_then(|c| c.finish_run(run_id, &std::sync::atomic::AtomicBool::new(false))) {
        tracing::info!("checkpoints: run {run_id} changed {changed} file(s) of its project");
    }
}

/// The full paths (a drive, a colon, a slash) the words of a request name: `E:\work`, `"E:\my folder\x.py"`, `e:/a/b.txt.`. A path with a space in it is found only when it is in quotes.
pub fn paths_named_in(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut found: Vec<String> = Vec::new();
    let mut i = 0;
    while i + 2 < chars.len() {
        let starts = chars[i].is_ascii_alphabetic() && chars[i + 1] == ':' && (chars[i + 2] == '\\' || chars[i + 2] == '/') && (i == 0 || !chars[i - 1].is_alphanumeric());
        if !starts {
            i += 1;
            continue;
        }
        let quote = if i > 0 && matches!(chars[i - 1], '"' | '\'' | '`') { Some(chars[i - 1]) } else { None };
        let mut j = i;
        while j < chars.len() {
            let c = chars[j];
            let stop = match quote {
                Some(q) => c == q || c == '\n',
                None => c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '<' | '>' | '|' | '?' | '*' | ',' | ';'),
            };
            if stop {
                break;
            }
            j += 1;
        }
        let raw: String = chars[i..j].iter().collect();
        let cleaned = raw.trim_end_matches(['.', ')', ']', '!', ':']).to_string();
        if cleaned.trim_end_matches(['\\', '/']).len() > 2 && !found.contains(&cleaned) {
            found.push(cleaned);
        }
        i = j.max(i + 1);
    }
    found
}

/// The path a refused file call named, out of the guard's own sentence (`'E:/x/y.txt' is outside the folders Aqua may use. ...`).
pub(super) fn refused_path(message: &str) -> Option<String> {
    let rest = message.strip_prefix('\'')?;
    let (path, _) = rest.split_once("' is outside the folders Aqua may use")?;
    Some(path.to_string())
}

/// The folder to ask the person about: of the paths their own words name, the longest one that holds the refused path (a name is taken whole, so `E:\vaibhav` does not hold
/// `E:/vaibhav_testing/x`), or, when they named the file itself, its folder. `None` when their words name nothing that holds it.
pub fn folder_to_allow(goal: &str, refused: &str) -> Option<String> {
    let norm = |p: &str| p.replace('\\', "/").trim_end_matches('/').to_ascii_lowercase();
    let np = norm(refused);
    let mut best: Option<(usize, String)> = None;
    for named in paths_named_in(goal) {
        let nn = norm(&named);
        let whole = named.trim_end_matches(['/', '\\']).to_string();
        let candidate = if np.starts_with(&format!("{nn}/")) {
            Some(whole)
        } else if np == nn {
            whole.rfind(['/', '\\']).map(|cut| whole[..cut].to_string()).filter(|parent| parent.trim_end_matches(['/', '\\']).len() > 2)
        } else {
            None
        };
        if let Some(c) = candidate {
            if best.as_ref().is_none_or(|(n, _)| nn.len() > *n) {
                best = Some((nn.len(), c));
            }
        }
    }
    best.map(|(_, c)| c)
}

/// A file tool was refused because its path is outside the folders Aqua may use, and the person's own request names a folder that holds it: ask the person to allow that folder
/// (`fs.allow_folder`, which always asks, in every mode) and, when they say yes, make the call again, once. What the agent reads is the answer to the call: the new result, or the
/// person's no. `None` when none of this applies, and the first result stands.
pub(super) fn after_a_refused_folder(host: &ToolHost, tool: &str, arguments: Value, res: &ToolResult, ctx: RequestContext, run_id: &str) -> Option<ToolResult> {
    if res.ok || tool == "fs.allow_folder" || run_id.is_empty() {
        return None;
    }
    let e = res.error.as_ref()?;
    if e.code != aqua_tool_host::ErrorCode::PermissionDenied {
        return None;
    }
    let refused = refused_path(&e.message)?;
    let folder = folder_to_allow(&crate::sources::goal_of(run_id), &refused)?;
    // One question per folder per run (the registry of the runs remembers which), so that a no is a no and not a question every round.
    if !registry().ask_folder_once(run_id, &folder) {
        return None;
    }
    let allow = host.call_blocking(ToolRequest { tool: "fs.allow_folder".into(), arguments: json!({ "path": folder, "access": "rw" }), context: ctx.clone() });
    if !allow.ok {
        // Not allowed: written down as a gap (card R7-05); it changes nothing here.
        crate::self_repair::note_folder_ask_failed(run_id, tool, &refused, &folder, &allow);
        return Some(allow);
    }
    Some(host.call_blocking(ToolRequest { tool: tool.to_string(), arguments, context: ctx }))
}

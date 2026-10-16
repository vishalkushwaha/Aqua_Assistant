//! Reading the config files again while Aqua runs, only for what cannot loosen anything (card OA11-03, task EX-13 of the OpenCode audit).
//!
//! Every document said "restart Aqua after editing it", so a skill approved in the app was unusable until a restart and a server added to `mcp.json` waited for one. `POST /api/v1/config/reload`
//! (UI token) and, with `AQUA_CONFIG_WATCH=on`, a look every two seconds at the files, read again what is below. **A reload never raises a trust level**:
//!
//! | what | what a reload does |
//! |---|---|
//! | `hooks` | `data/hooks.json` is read again. A file that is wrong asks about every call that is not a read (as at start). A file that has every hook that was running, and perhaps more, is put in place. A file that no longer has a hook that ran, or has it changed, is **not** applied: that loosens what is checked, and it waits for a restart (the answer says so) |
//! | `skills` | `data/skills` is read again (the approve route already does this). Skills are text for the prompt and can only narrow what a run may call |
//! | `mcp` | `data/tools/mcp.json` is read again. A server that is gone is stopped and its tools leave the host; one whose entry changed is stopped and is not trusted; a new one is listed and not trusted; one that is as it was is left running. A file that is wrong stops every server of the person's until it is fixed |
//! | `pack` | the `.aqua/mcp.json` of the projects that have servers is read again, with what the person trusts of each pack now: a pack that is not trusted any more has no servers |
//! | `commands`, `agents` | read from their folders at the start of every run: nothing to do |
//!
//! `policy.json`, `fs.json`, `term*.json`, `http.json`, `db.json`, `ssh.json`, the files of docker, kubernetes, infrastructure, cloud, forge and monitoring, and `remote_model.json` are **never**
//! read again: they say what Aqua may do, and loosening them is a thing for a restart, on purpose.
//!
//! Nothing is read again while a run is going on (a run that began with one set of rules does not end with another): the request is kept and done when the next run starts and is the only one.

use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

/// What can be asked to be read again.
pub const KINDS: &[&str] = &["hooks", "skills", "mcp", "pack", "commands", "agents"];

/// The files that are never read again while Aqua runs.
pub const NOT_RELOADABLE: &[&str] = &["policy.json", "fs.json", "term*.json", "http.json", "db.json", "ssh.json", "docker.json", "k8s.json", "iac.json", "cloud.json", "forge.json", "mon.json", "remote_model.json"];

/// How often the files are looked at with `AQUA_CONFIG_WATCH=on`.
pub const WATCH_EVERY: Duration = Duration::from_secs(2);

/// What was done for one kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    pub what: String,
    /// `applied`, `nothing_to_do`, `needs_restart`, `deferred` (a run is going on: it is done when the next run starts) or `failed`.
    pub state: &'static str,
    pub detail: String,
}

fn item(what: &str, state: &'static str, detail: impl Into<String>) -> Item {
    Item { what: what.to_string(), state, detail: detail.into() }
}

/// Reloads waiting for the run that is going on to end.
static PENDING: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

fn pending() -> std::sync::MutexGuard<'static, BTreeSet<String>> {
    PENDING.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Is a run going on?
fn busy() -> bool {
    crate::agent_tools::runs_in_flight() > 0 || crate::queue::foreground_active() > 0
}

/// Are the files looked at by themselves (`AQUA_CONFIG_WATCH=on`)?
pub fn watching() -> bool {
    crate::flags::is_on(&crate::flags::CONFIG_WATCH)
}

/// The kinds in `what` (all of them for none), or why a name is not one.
pub fn kinds_of(what: &[String]) -> Result<Vec<&'static str>, String> {
    if what.is_empty() {
        return Ok(KINDS.to_vec());
    }
    let mut out: Vec<&'static str> = Vec::new();
    for w in what {
        let name = w.trim().to_ascii_lowercase();
        match KINDS.iter().find(|k| **k == name) {
            Some(k) => {
                if !out.contains(k) {
                    out.push(k);
                }
            }
            None => return Err(format!("{w:?} is not something that is read again: {}", KINDS.join(", "))),
        }
    }
    Ok(out)
}

/// Reads `what` (names of [`KINDS`]; empty is all of them) again, or keeps the request for the next run when one is going on. `Err` names a kind that does not exist.
pub fn reload(data_dir: &Path, what: &[String]) -> Result<Vec<Item>, String> {
    let kinds = kinds_of(what)?;
    if busy() {
        let mut held = pending();
        let items = kinds.iter().map(|k| item(k, "deferred", "a run is going on: this is read again when the next run starts and is the only one")).collect();
        held.extend(kinds.iter().map(|k| k.to_string()));
        return Ok(items);
    }
    Ok(kinds.into_iter().map(|k| apply(data_dir, k)).collect())
}

/// Called when a run begins: the reloads that were asked for while another run was going on are done now if this run is the only one.
pub fn at_run_start() {
    if crate::agent_tools::runs_in_flight() > 1 {
        return;
    }
    let held: Vec<String> = std::mem::take(&mut *pending()).into_iter().collect();
    if held.is_empty() {
        return;
    }
    let data_dir = crate::runtime::data_dir();
    for k in kinds_of(&held).unwrap_or_default() {
        let done = apply(&data_dir, k);
        tracing::info!("config reload at the start of a run: {} {} ({})", done.what, done.state, done.detail);
    }
}

/// Which reloads are waiting (for the status).
pub fn pending_kinds() -> Vec<String> {
    pending().iter().cloned().collect()
}

fn apply(data_dir: &Path, kind: &'static str) -> Item {
    match kind {
        "hooks" => match crate::hooks::reload(data_dir) {
            crate::hooks::HooksReload::Applied { hooks } => item(kind, "applied", format!("{hooks} hook(s) from hooks.json are the ones that run now")),
            crate::hooks::HooksReload::Broken(why) => item(kind, "applied", format!("hooks.json is wrong ({why}): every call that is not a read asks until it is fixed")),
            crate::hooks::HooksReload::NeedsRestart(why) => item(kind, "needs_restart", why),
            crate::hooks::HooksReload::Unavailable => item(kind, "nothing_to_do", "the tool host is not running"),
        },
        "skills" => {
            let skills = crate::skills::reload(data_dir);
            item(kind, "applied", format!("{} skill(s); {} could not be read", skills.len(), skills.problems.len()))
        }
        "mcp" => match crate::mcp_exposure::servers() {
            None => item(kind, "nothing_to_do", "the tool host is not running"),
            Some(Err(why)) => item(kind, "needs_restart", format!("mcp.json was wrong when Aqua started ({why}): no server is used until it is fixed and Aqua is started again")),
            Some(Ok(manager)) => match manager.reload() {
                Ok(done) => {
                    if !done.to_start.is_empty() {
                        let (m, ids) = (manager.clone(), done.to_start.clone());
                        if let Err(e) = crate::jobs::spawn(crate::jobs::JobKind::Maintenance, "mcp-reload-start", None, move |_job| {
                            m.start_servers(&ids);
                            Ok(())
                        }) {
                            tracing::warn!("could not start the thread that starts the MCP servers again: {e}");
                        }
                    }
                    item(kind, "applied", format!("{} server(s) as they were, {} new (not trusted), {} changed (stopped, not trusted), {} gone (stopped), {} started again", done.kept.len(), done.added.len(), done.changed.len(), done.removed.len(), done.to_start.len()))
                }
                Err(why) => item(kind, "applied", format!("mcp.json is wrong ({why}): the servers of yours are stopped until it is fixed")),
            },
        },
        "pack" => {
            let n = crate::mcp_exposure::refresh_projects();
            item(kind, if n == 0 { "nothing_to_do" } else { "applied" }, format!("{n} project(s) with servers read again"))
        }
        "commands" | "agents" => item(kind, "nothing_to_do", "read from its folder at the start of every run"),
        other => item(other, "failed", "unknown"),
    }
}

// ---- the watch -----------------------------------------------------------------------------------------------------------------------------------------------------

/// What the watch looks at for each kind: the files and their last change.
fn signature(data_dir: &Path) -> Vec<(&'static str, Vec<(String, u128, u64)>)> {
    let stamp = |path: &Path| -> Option<(String, u128, u64)> {
        let m = std::fs::metadata(path).ok()?;
        let modified = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis();
        Some((path.to_string_lossy().to_string(), modified, m.len()))
    };
    let mut skills: Vec<(String, u128, u64)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(data_dir.join("skills")) {
        for e in entries.flatten() {
            if let Some(s) = stamp(&e.path().join("SKILL.md")) {
                skills.push(s);
            }
        }
        skills.sort();
    }
    vec![
        ("hooks", stamp(&crate::hooks::hooks_path(data_dir)).into_iter().collect()),
        ("mcp", stamp(&aqua_mcp_adapters::user_servers::ServerManager::config_path(&data_dir.join("tools"))).into_iter().collect()),
        ("skills", skills),
    ]
}

/// The kinds whose files differ between two looks.
fn changed_kinds(before: &[(&'static str, Vec<(String, u128, u64)>)], now: &[(&'static str, Vec<(String, u128, u64)>)]) -> Vec<String> {
    now.iter().filter(|(k, files)| before.iter().find(|(b, _)| b == k).map_or(!files.is_empty(), |(_, old)| old != files)).map(|(k, _)| k.to_string()).collect()
}

/// With `AQUA_CONFIG_WATCH=on`: a job that looks at the files every [`WATCH_EVERY`] and reads one kind again when its files changed and have been the same for one more look (a file that is
/// being written is not read half-way). Without the switch it does nothing.
pub fn start_watch(data_dir: &Path) {
    if !watching() {
        return;
    }
    let data_dir = data_dir.to_path_buf();
    let started = crate::jobs::spawn(crate::jobs::JobKind::Watcher, "aqua-config-watch", Some(Duration::from_secs(10)), move |job| {
        let mut known = signature(&data_dir);
        let mut waiting: Option<Vec<(&'static str, Vec<(String, u128, u64)>)>> = None;
        while job.sleep(WATCH_EVERY) {
            let now = signature(&data_dir);
            if now == known {
                waiting = None;
                continue;
            }
            // The files differ from what was read: they are read once they have stayed the same for one more look.
            if waiting.as_ref() != Some(&now) {
                waiting = Some(now);
                continue;
            }
            let kinds = changed_kinds(&known, &now);
            known = now;
            waiting = None;
            if !kinds.is_empty() {
                match reload(&data_dir, &kinds) {
                    Ok(items) => {
                        for i in items {
                            tracing::info!("config changed on disk, {} {} ({})", i.what, i.state, i.detail);
                        }
                    }
                    Err(why) => tracing::warn!("config changed on disk, and could not be read again: {why}"),
                }
            }
        }
        Ok(())
    });
    if let Err(e) = started {
        tracing::warn!("could not start the thread that watches the config files: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_cfg_reload_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_kinds_are_checked_and_empty_is_all_of_them() {
        assert_eq!(kinds_of(&[]).unwrap(), KINDS.to_vec());
        assert_eq!(kinds_of(&["MCP".to_string(), " hooks ".to_string(), "mcp".to_string()]).unwrap(), vec!["mcp", "hooks"]);
        let said = kinds_of(&["policy".to_string()]).unwrap_err();
        assert!(said.contains("not something that is read again") && said.contains("hooks, skills, mcp"), "{said}");
        assert!(kinds_of(&["policy.json".to_string()]).is_err(), "the files that say what Aqua may do are not on the list");
        for never in ["policy.json", "fs.json", "term*.json", "http.json", "remote_model.json"] {
            assert!(NOT_RELOADABLE.contains(&never), "{never}");
        }
    }

    #[test]
    fn a_change_of_a_file_is_noticed_by_kind() {
        let d = temp("signature");
        std::fs::create_dir_all(d.join("tools")).unwrap();
        std::fs::create_dir_all(d.join("skills/one")).unwrap();
        let before = signature(&d);
        assert!(changed_kinds(&before, &signature(&d)).is_empty(), "nothing changed");
        std::fs::write(d.join("hooks.json"), r#"{"hooks": []}"#).unwrap();
        assert_eq!(changed_kinds(&before, &signature(&d)), vec!["hooks".to_string()], "a file that was not there");
        std::fs::write(d.join("skills/one/SKILL.md"), "---\nname: one\n---\nx").unwrap();
        std::fs::write(d.join("tools/mcp.json"), r#"{"mcpServers": {}}"#).unwrap();
        let now = signature(&d);
        let mut kinds = changed_kinds(&before, &now);
        kinds.sort();
        assert_eq!(kinds, vec!["hooks".to_string(), "mcp".to_string(), "skills".to_string()]);
        std::fs::remove_file(d.join("hooks.json")).unwrap();
        assert!(changed_kinds(&now, &signature(&d)).contains(&"hooks".to_string()), "a file that went");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The hooks of the file, put in place of the person's, with the rule that nothing that ran is given up.
    #[test]
    fn a_hook_that_ran_is_never_taken_away_by_a_reload_and_a_wrong_file_asks_about_everything() {
        use crate::hooks::{reload_into, HooksReload, PackAware, Running};
        use aqua_tool_host::hooks::{HookAnswer, HookCall, Hooks};
        let d = temp("hooks");
        let write = |text: &str| std::fs::write(d.join("hooks.json"), text).unwrap();
        let call = |tool: &str| HookCall { event: "PreToolUse", tool: tool.into(), classes: vec!["WRITE".into()], decision: "auto", caller: "agent".into(), ..Default::default() };
        let one = |tool: &str| format!(r#"{{"event": "PreToolUse", "tools": ["{tool}"], "command": ["definitely-not-a-program-aqua-test"]}}"#);
        let deny_all = |tool: &str| format!(r#"{{"hooks": [{}]}}"#, one(tool));

        // Started with no file: nothing runs. A file with a hook is put in place at once (more checks).
        let aware = PackAware::new(None);
        let mut running = Running::Nothing;
        assert!(matches!(aware.pre_tool(&call("fs.write")), HookAnswer::Pass));
        write(&deny_all("fs.write"));
        assert_eq!(reload_into(&aware, &mut running, &d), HooksReload::Applied { hooks: 1 });
        assert!(matches!(aware.pre_tool(&call("fs.write")), HookAnswer::Ask(_) | HookAnswer::Deny(_)), "the new hook is in force on the next call (it cannot run, so it asks)");
        assert!(matches!(aware.pre_tool(&call("fs.read")), HookAnswer::Pass), "and only for the tools it names");

        // The same hook and one more: applied.
        write(&format!(r#"{{"hooks": [{}, {}]}}"#, one("fs.write"), one("fs.delete")));
        assert_eq!(reload_into(&aware, &mut running, &d), HooksReload::Applied { hooks: 2 });
        assert!(!matches!(aware.pre_tool(&call("fs.delete")), HookAnswer::Pass));

        // A hook that ran is changed, or gone: not applied, and the hooks that run are the ones that ran.
        write(&deny_all("fs.delete"));
        let refused = reload_into(&aware, &mut running, &d);
        assert!(matches!(&refused, HooksReload::NeedsRestart(w) if w.contains("loosens")), "{refused:?}");
        assert!(!matches!(aware.pre_tool(&call("fs.write")), HookAnswer::Pass), "the hook for fs.write is still in force");
        write(r#"{"hooks": []}"#);
        assert!(matches!(reload_into(&aware, &mut running, &d), HooksReload::NeedsRestart(_)), "an empty file does not take the hooks away");
        std::fs::remove_file(d.join("hooks.json")).unwrap();
        assert!(matches!(reload_into(&aware, &mut running, &d), HooksReload::NeedsRestart(_)), "nor does a file that is gone");

        // A wrong file: stricter than before, at once, for everything that is not a read.
        write("{ not json");
        assert!(matches!(reload_into(&aware, &mut running, &d), HooksReload::Broken(_)));
        assert!(matches!(aware.pre_tool(&call("fs.write")), HookAnswer::Ask(_)));
        assert!(matches!(aware.pre_tool(&HookCall { classes: vec!["READ".into()], ..call("fs.read") }), HookAnswer::Pass), "a read still goes");
        // Fixed: the person's own file is back in force (it was what the person meant, and the state before was the strict one).
        write(&deny_all("term.run"));
        assert_eq!(reload_into(&aware, &mut running, &d), HooksReload::Applied { hooks: 1 });
        let _ = std::fs::remove_dir_all(&d);
    }
}

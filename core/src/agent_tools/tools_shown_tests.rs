//! Tests of `tools_shown.rs`: which tools a run is shown for its goal, the names a model writes for them and what a tool can do (move only: they were part of the inline module `tests` of `agent_tools.rs`; the path of every test is now `agent_tools::tools_shown_tests::<name>`).

use super::tests::{host, real_host};
use super::*;
use aqua_tool_host::*;
use serde_json::json;

/// Card OA11-02: a command that names no pack is shown the packs of the tools it allows, so that a short list is not cut by the places of a pack the command has no use for.
#[test]
fn a_command_that_names_no_pack_is_shown_the_packs_of_the_tools_it_allows() {
    let h = Arc::new(host());
    let shown = |allowed: &[&str], packs: &[&str]| -> Vec<String> {
        let options = RunOptions { allowed_tools: allowed.iter().map(|s| s.to_string()).collect(), forced_packs: packs.iter().map(|s| s.to_string()).collect(), tainted: false };
        with_run_options(options, || {
            let _scope = RunScope::begin(Some(h.clone()));
            tools_for_goal(&h, "anything at all").into_iter().map(|t| t.name).collect()
        })
    };
    let mut names = shown(&["fs.*"], &[]);
    assert!(!names.is_empty() && names.iter().all(|n| n.starts_with("fs.")), "{names:?}");
    names = shown(&["git.status", "web.fetch"], &[]);
    names.sort();
    assert_eq!(names, vec!["git.status", "web.fetch"]);
    // A pack that does not exist is not one; with nothing else to go by the words of the goal decide, as for any request.
    assert!(shown(&[], &["nothing_like_it"]).is_empty());
    // A command that allows nothing and names nothing narrows nothing at all.
    assert!(shown(&[], &[]).is_empty());
}

/// A reminders service that holds nothing: the provider is the real one, and only its tools matter here.
struct NoReminders;
impl crate::reminders::ReminderApi for NoReminders {
    fn add(&self, _message: &str, _due: &crate::reminders::Due) -> Result<crate::reminders::ReminderView, String> {
        Err("not here".into())
    }
    fn list(&self) -> Result<Vec<crate::reminders::ReminderView>, String> {
        Ok(Vec::new())
    }
    fn cancel(&self, _id: i64) -> Result<bool, String> {
        Ok(false)
    }
}

#[test]
fn the_tools_of_the_vault_the_reminders_and_the_memory_are_shown_for_a_goal_that_calls_for_them() {
    let dir = std::env::temp_dir().join(format!("aqua_agent_tools_packs_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let h = ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(dir.join("audit.jsonl"))));
    h.register(Arc::new(crate::vault::tools::VaultProvider(Arc::new(crate::vault::Vault::open_at(&dir.join("vault.db")).unwrap())))).unwrap();
    h.register(Arc::new(crate::reminders::ReminderProvider(Arc::new(NoReminders)))).unwrap();
    h.register(Arc::new(crate::memory::user_facts::provider::UserFactsProvider::new(Arc::new(crate::memory::user_facts::store::FactStore::open(dir.join("facts.db")).unwrap())))).unwrap();
    let shown = |goal: &str| -> Vec<String> { tools_for_goal_with(&h, goal, true).into_iter().map(|t| t.name).collect() };
    let vault = shown("search my notes for the launch code");
    assert!(vault.contains(&"vault.search".to_string()), "{vault:?}");
    let set = shown("remind me to call Anna in twenty minutes");
    assert!(set.contains(&"reminder.add".to_string()) && set.contains(&"reminder.list".to_string()), "{set:?}");
    assert!(shown("cancel my timer").contains(&"reminder.cancel".to_string()));
    let keep = shown("remember that my sister is called Asha");
    assert!(keep.contains(&"memory.remember".to_string()) && keep.contains(&"memory.search".to_string()), "{keep:?}");
    assert!(shown("forget what I told you about my address").contains(&"memory.forget".to_string()));
    // Every tool these providers register can be shown for some goal: a pack with the prefix exists.
    for m in h.manifests() {
        assert!(crate::tool_providers::packs().iter().any(|p| p.prefixes.iter().any(|pre| m.name.starts_with(pre))), "{} has no pack, so the agent is never shown it", m.name);
    }
    // The packs did not take what they should not: a coding request and a plain question still get what they got.
    assert_eq!(route("read the file notes.txt in my folder"), vec!["files"]);
    assert!(!route("list the processes using memory").contains(&"memory"));
}

#[test]
fn shown_tools_are_capped_read_first_and_carry_their_arguments() {
    let h = host();
    assert!(tools_for_goal(&h, "open notepad").is_empty());
    let t = tools_for_goal(&h, "read the file a.txt");
    assert_eq!(t.len(), MAX_HOST_TOOLS, "capped");
    assert!(t.iter().all(|x| x.source.as_deref() == Some(SOURCE) && x.name.starts_with("fs.")));
    assert!(!t.iter().any(|x| x.name == "fs.write_file"), "read tools come first, so the cap drops writes before reads");
    let read = t.iter().find(|x| x.name == "fs.read_file").unwrap();
    assert!(read.description.starts_with("(path, limit?)"), "{}", read.description);
    let two = tools_for_goal(&h, "git status of the repo and read the file x.txt");
    assert!(two.iter().any(|x| x.name == "git.status") && two.iter().any(|x| x.name.starts_with("fs.")));
}

#[test]
fn a_tool_can_change_something_unless_it_is_known_to_only_look() {
    let host: Option<&ToolHost> = None;
    // Not the host's: only the few names known to look are let through, and a name that merely contains a word like "get" is not one of them.
    for looks in ["Snapshot", "snapshot", "Screenshot", "browser_snapshot", "browser_find", "aqua_browser_extract", "local_find"] {
        assert!(!can_change_something(host, looks), "{looks}");
    }
    for changes in ["Click", "Type", "App", "browser_click", "browser_navigate", "get_and_send", "read_and_delete", "snapshot_and_click", "mcp_x.list_and_post", "local_open"] {
        assert!(can_change_something(host, changes), "{changes}");
    }
}

#[test]
fn a_tool_that_was_cut_from_a_shown_pack_is_still_the_hosts_to_decide_on_and_one_from_a_pack_that_was_not_shown_is_not_there() {
    let h = real_host();
    // The files pack was shown, but under the earlier order it ran out of room before `fs.edit` and `fs.write`.
    let goal = "fix the bug in notes.txt and commit it to the repo";
    let shown = tools_for_goal_with(&h, goal, false);
    assert!(shown.iter().any(|t| t.name.starts_with("fs.")) && !shown.iter().any(|t| t.name == "fs.edit"), "setup: a files pack with fs.edit cut off");
    for spelling in ["fs.edit", "fs_edit", "FS.EDIT", "host.fs.edit", "aqua_fs_edit"] {
        assert_eq!(canonical_hidden(spelling, &h, &shown, false).as_deref(), Some("fs.edit"), "{spelling}");
    }
    // A pack that was not shown for this goal cannot be reached by naming a tool of it: a goal about files cannot call `docker.*`
    // or `term.*` it was not given, and a desktop goal, which is shown no host tool at all, cannot call anything of the host.
    assert_eq!(canonical_hidden("term.run", &h, &tools_for_goal(&h, "read the file notes.txt"), false), None, "terminal tools were not shown for a files goal");
    let desktop = tools_for_goal(&h, "open notepad");
    assert!(desktop.is_empty(), "nothing is shown for a desktop goal");
    for name in ["fs.edit", "fs_write", "term.run", "git.push"] {
        assert_eq!(canonical_hidden(name, &h, &desktop, false), None, "{name}");
    }
    // Names that are not host tools stay what they were: a desktop or browser tool, or unknown.
    for other in ["Click", "App", "browser_navigate", "aqua_browser_extract", "multi_select", "fs.delete_everything", "nope", ""] {
        assert_eq!(canonical_hidden(other, &h, &shown, false), None, "{other}");
    }
    // The call itself is the host's: a voice request cannot write, shown or not.
    let voice = call(&h, "fs.write", json!({"path": "a"}), Caller::Voice, "r9");
    assert!(voice.starts_with("ERROR"), "{voice}");
}

#[test]
fn fs_edit_is_not_overflow() {
    let h = real_host();
    // With two packs each gets six places. Before, the files pack spent them on the four read tools, then `fs.copy` and
    // `fs.delete`, and ran out of room before `fs.edit` and `fs.write`.
    let goal = "fix the bug in notes.txt and commit it to the repo";
    let old: Vec<String> = tools_for_goal_with(&h, goal, false).into_iter().map(|t| t.name).collect();
    assert!(!old.iter().any(|n| n == "fs.edit" || n == "fs.write"), "the old order really did drop them: {old:?}");
    let now: Vec<String> = tools_for_goal_with(&h, goal, true).into_iter().map(|t| t.name).collect();
    assert!(now.iter().any(|n| n == "fs.edit") && now.iter().any(|n| n == "fs.write"), "{now:?}");
}

#[test]
fn model_spellings_resolve_to_the_shown_tool_and_nothing_else() {
    let h = host();
    let t = tools_for_goal(&h, "read the file a.txt");
    for n in ["fs.read_file", "fs_read_file", "FS.READ_FILE", "host.fs.read_file", "fs-read-file", "aqua_fs_read_file", "fs.read.file", " fs.read_file "] {
        assert_eq!(canonical(n, &t).as_deref(), Some("fs.read_file"), "{n}");
    }
    for n in ["fs.delete_everything", "read_file", "Click", "term.run", ""] {
        assert_eq!(canonical(n, &t), None, "{n}");
    }
}

/// Card R6-08 with R4-03: the clipboard tools are offered for a goal about the clipboard, and only where the host has them (`AQUA_CLIPBOARD_TOOLS=on` registers them): a goal
/// that is about something else is not shown them, and a host that did not register them has nothing to show.
#[test]
fn a_goal_about_the_clipboard_is_shown_the_clipboard_tools_where_the_host_has_them_and_no_other_goal_is() {
    struct Held;
    impl aqua_native_tools::clipboard::ClipboardSource for Held {
        fn read_text(&self) -> Result<Option<String>, String> {
            Ok(Some("held".into()))
        }
        fn write_text(&self, _text: &str) -> Result<(), String> {
            Ok(())
        }
    }
    assert_eq!((pack_of("clipboard.read"), pack_of("clipboard.write")), (Some("clipboard"), Some("clipboard")));
    for goal in ["read what is on my clipboard", "paste the clipboard into my notes", "summarise what I just copied"] {
        assert!(route_with(goal, false).contains(&"clipboard"), "{goal}: {:?}", route_with(goal, false));
    }
    for goal in ["save the file notes.txt to my disk", "what's the weather in Delhi", "open notepad and type hello", "list the open issues on my github repo", "copy report.docx to the backup folder"] {
        assert!(!route_with(goal, false).contains(&"clipboard"), "{goal}: {:?}", route_with(goal, false));
    }
    let dir = std::env::temp_dir().join(format!("aqua_agent_tools_clipboard_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let with = ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(dir.join("audit.jsonl"))));
    with.register(Arc::new(aqua_native_tools::clipboard::ClipboardProvider::new(Arc::new(Held)))).unwrap();
    let names = |h: &ToolHost| -> Vec<String> { tools_for_goal_with(h, "what is on my clipboard", false).into_iter().map(|t| t.name).collect() };
    let mut shown = names(&with);
    shown.sort();
    assert_eq!(shown, vec!["clipboard.read".to_string(), "clipboard.write".to_string()]);
    assert!(names(&host()).is_empty(), "a host that did not register the clipboard tools has nothing to show");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_tools_of_a_server_the_user_added_are_shown_for_a_goal_that_names_it_and_only_with_their_arguments() {
    struct Notes;
    impl ToolProvider for Notes {
        fn id(&self) -> &str {
            "mcp_notes"
        }
        fn manifests(&self) -> Vec<ToolManifest> {
            vec![ToolManifest {
                name: "mcp_notes.add_note".into(),
                title: "mcp_notes.add_note".into(),
                description: "A tool of the MCP server notes you added: add-note.".into(),
                classes: vec![PermClass::Remote, PermClass::ExternalEffect],
                default_decision: Decision::Ask,
                input_schema: json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}),
                timeout_default_ms: 1000,
                timeout_max_ms: 1000,
                annotations: Annotations::default(),
                untrusted_output: true,
            }]
        }
        fn classify(&self, _t: &str, _a: &Value) -> Result<Classification, ToolError> {
            Ok(Classification { classes: vec![], target: "notes".into(), summary: "s".into(), target_allowed: false })
        }
        fn call<'a>(&'a self, _t: &'a str, _a: Value, _c: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
            Box::pin(async { Ok(ProviderOutput::default()) })
        }
    }
    let h = host();
    h.register(Arc::new(Notes)).unwrap();
    let names = |goal: &str| tools_for_goal(&h, goal).into_iter().map(|t| t.name).collect::<Vec<_>>();
    assert!(names("add a note in notes about the meeting").contains(&"mcp_notes.add_note".to_string()));
    assert!(names("Add this to NOTES please").contains(&"mcp_notes.add_note".to_string()));
    assert!(!names("what is the weather in pune").contains(&"mcp_notes.add_note".to_string()));
    assert!(!names("add footnotes to the book").contains(&"mcp_notes.add_note".to_string()), "a word that only contains the name is not the name");
    let shown = tools_for_goal(&h, "use notes to add a reminder").into_iter().find(|t| t.name == "mcp_notes.add_note").unwrap();
    assert!(shown.description.starts_with("(text: string)") && !shown.description.contains("IGNORE"), "{}", shown.description);
    assert!(has_word("add a note in notes", "notes") && !has_word("denotes", "notes") && !has_word("notes2", "notes") && !has_word("x", ""));
}

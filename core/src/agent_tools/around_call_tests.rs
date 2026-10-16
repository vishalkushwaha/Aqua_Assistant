//! Tests of `around_call.rs`: the folder a person named, and the look at a project before a command (move only: they were part of the inline module `tests` of `agent_tools.rs`; the path of every test is now `agent_tools::around_call_tests::<name>`).

use super::*;
use aqua_tool_host::*;
use serde_json::json;
use std::sync::Mutex;

// ---- a folder the person named (found on 2026-10-08: "write a hello world in E:\vaibhav_testing\ folder" could not be written, the folder was outside the allowed ones) ----

#[test]
fn the_paths_a_request_names_are_found_with_or_without_quotes_and_the_folder_to_ask_about_is_the_one_that_holds_the_refused_path() {
    let goal = r#"can u write a hello world for me in python in "E:\vaibhav_testing\" folder for me"#;
    assert_eq!(paths_named_in(goal), vec![r"E:\vaibhav_testing\".to_string()]);
    assert_eq!(paths_named_in(r#"copy "D:\my folder\x.py" to E:\y, then save e:/a/b.txt."#), vec![r"D:\my folder\x.py".to_string(), r"E:\y".to_string(), "e:/a/b.txt".to_string()]);
    assert!(paths_named_in("see http://example.com/a and 1:2 and C:").is_empty(), "an address, a ratio and a bare drive are not paths");
    // The refused path is named by the guard's own sentence.
    assert_eq!(refused_path("'E:/vaibhav_testing/hello.py' is outside the folders Aqua may use. Use a path inside one of: @ws = C:/x").as_deref(), Some("E:/vaibhav_testing/hello.py"));
    assert_eq!(refused_path("'@ws' is read-only"), None);
    // The folder the words name that holds it: the longest, whole names only, or the folder of a file they named.
    assert_eq!(folder_to_allow(goal, "E:/vaibhav_testing/hello.py").as_deref(), Some(r"E:\vaibhav_testing"));
    assert_eq!(folder_to_allow(r"put it in E:\a and E:\a\b please", "E:/a/b/c.txt").as_deref(), Some(r"E:\a\b"));
    assert_eq!(folder_to_allow(r"save it as E:\x\y.txt", "E:/x/y.txt").as_deref(), Some(r"E:\x"));
    assert_eq!(folder_to_allow(r"save it in E:\vaibhav", "E:/vaibhav_testing/x.txt"), None, "a name is taken whole");
    assert_eq!(folder_to_allow("write hello world in python", "E:/vaibhav_testing/x.txt"), None, "the person named no folder");
    assert_eq!(folder_to_allow(r"save it as E:\y.txt", "E:/y.txt"), None, "the folder of that file is a whole drive");
}

struct Answer {
    seen: Mutex<Vec<String>>,
    yes: bool,
}

impl aqua_tool_host::Approver for Answer {
    fn request(&self, req: aqua_tool_host::ApprovalRequest) -> ProviderFuture<'_, aqua_tool_host::ApprovalOutcome> {
        self.seen.lock().unwrap().push(req.summary.clone());
        let out = if self.yes { aqua_tool_host::ApprovalOutcome::Approved(aqua_tool_host::ApprovalScope::Once) } else { aqua_tool_host::ApprovalOutcome::Denied };
        Box::pin(async move { out })
    }
}

/// A host with the real file tools, one trusted folder, and a person who answers `yes` to every question.
fn fs_host(name: &str, yes: bool) -> (ToolHost, std::path::PathBuf, Arc<Answer>) {
    use aqua_native_tools::fs::{Access, FsConfig, FsProvider, PathGuard, Root, Trash};
    let dir = std::env::temp_dir().join(format!("aqua_agent_folder_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for d in ["ws", "outside", "aqua/tools"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    let cfg = FsConfig { roots: vec![Root { alias: "ws".into(), path: dir.join("ws").to_string_lossy().into(), access: Access::Rw, trusted: true }], ..FsConfig::with_workspace("unused") };
    let guard = Arc::new(PathGuard::new(cfg, vec![dir.join("aqua")]).unwrap());
    guard.set_kept(vec![]); // the temporary folder of a test run is inside AppData
    let trash = Arc::new(Trash::new(dir.join("aqua").join("trash")).unwrap());
    let host = ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(dir.join("aqua/tools/audit.jsonl"))));
    let answer = Arc::new(Answer { seen: Mutex::new(vec![]), yes });
    host.set_approver(answer.clone());
    host.register(Arc::new(FsProvider::new(guard, trash))).unwrap();
    (host, dir, answer)
}

#[test]
fn a_folder_the_person_named_is_asked_about_and_after_a_yes_the_call_goes_through() {
    let (h, dir, answer) = fs_host("yes", true);
    let outside = dir.join("outside");
    let goal = format!("can u write a hello world for me in python in \"{}\\\" folder for me", outside.display());
    crate::sources::begin("run_folder_yes", &goal);
    let file = format!("{}\\hello.py", outside.display());
    let said = call(&h, "fs.write", json!({"path": file, "content": "print('hello world')\n"}), Caller::Agent, "run_folder_yes");
    assert!(!said.starts_with("ERROR"), "{said}");
    assert_eq!(std::fs::read_to_string(outside.join("hello.py")).unwrap(), "print('hello world')\n");
    {
        let seen = answer.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen[0].starts_with("Allow Aqua's file tools to read and write files in the folder "), "the folder is asked about first: {:?}", seen[0]);
        assert!(seen[1].starts_with("Write "), "and the change asks as well: {:?}", seen[1]);
    }
    // The next file in it asks only about the change.
    let more = call(&h, "fs.write", json!({"path": format!("{}\\two.py", outside.display()), "content": "x = 1\n"}), Caller::Agent, "run_folder_yes");
    assert!(!more.starts_with("ERROR"), "{more}");
    assert_eq!(answer.seen.lock().unwrap().len(), 3);
    // A folder the words did not name is still outside, and nobody is asked.
    let other = call(&h, "fs.write", json!({"path": format!("{}\\x.txt", dir.join("elsewhere").display()), "content": "x"}), Caller::Agent, "run_folder_yes");
    assert!(other.starts_with("ERROR [INVALID_ARGUMENTS] fs.write:") && other.contains("fs.allow_folder"), "{other}");
    assert_eq!(answer.seen.lock().unwrap().len(), 3);
    crate::sources::end("run_folder_yes");
}

#[test]
fn a_no_is_a_no_and_is_not_asked_again_in_the_same_run() {
    let (h, dir, answer) = fs_host("no", false);
    let outside = dir.join("outside");
    crate::sources::begin("run_folder_no", &format!("write a hello world in {}", outside.display()));
    let file = format!("{}\\hello.py", outside.display());
    let first = call(&h, "fs.write", json!({"path": file, "content": "x"}), Caller::Agent, "run_folder_no");
    assert!(first.starts_with("ERROR [CONFIRMATION_DENIED] fs.allow_folder"), "{first}");
    assert!(!outside.join("hello.py").exists());
    let second = call(&h, "fs.write", json!({"path": file, "content": "x"}), Caller::Agent, "run_folder_no");
    assert!(second.starts_with("ERROR [INVALID_ARGUMENTS] fs.write:"), "the folder is not asked about twice: {second}");
    assert_eq!(answer.seen.lock().unwrap().len(), 1);
    crate::sources::end("run_folder_no");
}

#[test]
fn a_request_that_names_no_folder_is_never_turned_into_a_question() {
    let (h, dir, answer) = fs_host("none", true);
    let outside = dir.join("outside");
    crate::sources::begin("run_folder_none", "write a hello world in python");
    let file = format!("{}\\hello.py", outside.display());
    let said = call(&h, "fs.write", json!({"path": file, "content": "x"}), Caller::Agent, "run_folder_none");
    assert!(said.starts_with("ERROR [INVALID_ARGUMENTS] fs.write:"), "{said}");
    assert!(answer.seen.lock().unwrap().is_empty());
    crate::sources::end("run_folder_none");
    // A run with no goal on record (a call outside a run) is not escalated either.
    let said = call(&h, "fs.write", json!({"path": file, "content": "x"}), Caller::Agent, "");
    assert!(said.starts_with("ERROR"), "{said}");
    assert!(answer.seen.lock().unwrap().is_empty());
}

/// Card OA7-03 (ED-11, CX-11): a checkpoint of the project is taken before the first command of a run that may change files, once, only for a trusted project folder, and the second
/// look at the end of the run settles the record (taken away when nothing changed). Which calls count, and the switch, are decided before any look is made.
#[test]
fn a_checkpoint_is_taken_before_the_first_command_that_may_change_files_and_only_for_a_trusted_project() {
    use aqua_native_tools::fs::{Access, FsConfig, PathGuard, Root, Trash};
    use aqua_native_tools::git::checkpoint::Checkpoints;
    let dir = std::env::temp_dir().join(format!("aqua_agent_tools_checkpoint_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for d in ["ws/proj", "notes/proj", "aqua"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    std::fs::write(dir.join("ws/proj/a.txt"), "before").unwrap();
    std::fs::write(dir.join("notes/proj/a.txt"), "before").unwrap();
    let cfg = FsConfig {
        roots: vec![
            Root { alias: "ws".into(), path: dir.join("ws").to_string_lossy().into(), access: Access::Rw, trusted: true },
            Root { alias: "notes".into(), path: dir.join("notes").to_string_lossy().into(), access: Access::Rw, trusted: false },
        ],
        ..FsConfig::with_workspace("unused")
    };
    let guard = Arc::new(PathGuard::new(cfg, vec![dir.join("aqua")]).unwrap());
    let trash = Arc::new(Trash::new(dir.join("aqua/trash")).unwrap());
    let checkpoints = Checkpoints::new(&None, dir.join("aqua/snapshots"), guard, trash.clone());
    let entries = |run: &str| trash.run_entries(run).into_iter().filter(|e| e.reason == "checkpoint").count();
    let proj = Some(dir.join("ws/proj"));
    let fmt = json!({"exe": "cargo", "args": ["fmt"]});
    // Calls that cannot change files start nothing: a read, a file tool (it has its own records), a command that only looks, a commit.
    for (tool, args) in [("fs.read", json!({})), ("fs.write", json!({})), ("term.run", json!({"exe": "git", "args": ["status"]})), ("git.commit", json!({})), ("term.output", json!({}))] {
        checkpoint_before_command_in(Some(&checkpoints), proj.clone(), tool, &args, "run_cp_1");
    }
    assert_eq!(entries("run_cp_1"), 0);
    // No checkpoints (the file tools are off) and no folder: nothing happens either.
    checkpoint_before_command_in(None, proj.clone(), "term.run", &fmt, "run_cp_1");
    assert_eq!(entries("run_cp_1"), 0);
    // The first command that may change files takes the look, once.
    checkpoint_before_command_in(Some(&checkpoints), proj.clone(), "term.run", &fmt, "run_cp_1");
    checkpoint_before_command_in(Some(&checkpoints), proj.clone(), "term.shell", &json!({"command": "cargo fmt && cargo test"}), "run_cp_1");
    assert_eq!(entries("run_cp_1"), 1, "one record for a run, however many commands it runs");
    // A folder that is allowed for a request and not trusted gets none; the call is not stopped by it.
    checkpoint_before_command_in(Some(&checkpoints), Some(dir.join("notes/proj")), "term.run", &fmt, "run_cp_2");
    assert_eq!(entries("run_cp_2"), 0);
    // A run with no project of its own is looked at where its command says it runs.
    checkpoint_before_command_in(Some(&checkpoints), None, "term.run", &json!({"exe": "cargo", "args": ["fmt"], "cwd": "@ws/proj"}), "run_cp_3");
    assert_eq!(entries("run_cp_3"), 1);
    // The end of the run: what changed is recorded with the run, and a run that changed nothing leaves no record.
    std::fs::write(dir.join("ws/proj/a.txt"), "after").unwrap();
    finish_checkpoint_in(Some(&checkpoints), "run_cp_1");
    let kept = trash.run_entries("run_cp_1");
    assert!(kept.len() == 1 && kept[0].effect.as_ref().is_some_and(|e| e.start.is_some() && e.end.is_some() && e.changed == 1), "{kept:?}");
    // Run 3 began its look before the file changed too, so it saw the same change; a second end of the same run does nothing.
    finish_checkpoint_in(Some(&checkpoints), "run_cp_3");
    assert_eq!(trash.run_entries("run_cp_3").len(), 1);
    finish_checkpoint_in(Some(&checkpoints), "run_cp_3");
    assert_eq!(trash.run_entries("run_cp_3").len(), 1);
    finish_checkpoint_in(Some(&checkpoints), "run_never_started");
    // A run that began its look and changed nothing leaves no record at all.
    checkpoint_before_command_in(Some(&checkpoints), proj.clone(), "term.run", &fmt, "run_cp_5");
    assert_eq!(entries("run_cp_5"), 1);
    finish_checkpoint_in(Some(&checkpoints), "run_cp_5");
    assert_eq!(entries("run_cp_5"), 0, "nothing changed since its look: nothing to put back");
    // The switch is read where the call is made: with it off the public entry point does nothing.
    crate::flags::with_overrides(&[("AQUA_SNAPSHOT", None)], || checkpoint_before_command("term.run", &fmt, "run_cp_4"));
    assert_eq!(entries("run_cp_4"), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

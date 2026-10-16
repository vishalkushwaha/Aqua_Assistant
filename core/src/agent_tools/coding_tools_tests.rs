//! Tests of `coding_tools.rs`: the tools a coding goal is shown, and that each thing a coding run has to do is reachable from them (move only: they were part of the inline module `tests` of `agent_tools.rs`; the path of every test is now `agent_tools::coding_tools_tests::<name>`).

use super::tests::{real_host, shown_names, wide_host};
use super::*;
use aqua_tool_host::*;
use std::sync::atomic::Ordering;

#[test]
fn a_coding_goal_shows_the_six_tools() {
    let h = real_host();
    for goal in [
        "fix the failing test in rust_probe",
        "implement caching in utils.py",
        "refactor the parser module",
        "edit my notes",
        "update the README.md with install steps",
        "run cargo test and tell me what fails",
        "could you please fix the type error in app.tsx",
        "commit the fix to the repo after the tests pass",
        // (card OA4-01: the requests that name a word of code and no file)
        "implement pagination for the users endpoint in src/api.rs",
        "rename foo to bar across the repo",
        "make the build pass",
        "upgrade the dependencies of the project",
        "add an endpoint to the api folder",
    ] {
        let names: Vec<String> = tools_for_goal(&h, goal).into_iter().map(|t| t.name).collect();
        for must in ["fs.find", "fs.read", "fs.edit", "fs.write", "term.run"] {
            assert!(names.iter().any(|n| n == must), "{goal}: {must} is not shown: {names:?}");
        }
        // (card OA5-01, TM-08) `term.start` and `term.output` are shown together or not at all: this list used to keep `term.output` after `term.start` was dropped for room, which left a tool
        // that reads the output of a job the run could not start.
        assert_eq!(names.iter().any(|n| n == "term.start"), names.iter().any(|n| n == "term.output"), "{goal}: {names:?}");
        assert!(names.len() <= MAX_HOST_TOOLS, "{names:?}");
        if crate::laya_validator::goal_is_coding_or_files(goal) {
            for must in crate::modules::coding_session::PINNED_TOOLS {
                assert!(names.iter().any(|n| n == must), "{goal}: pinned {must} missing: {names:?}");
            }
        }
    }
}

#[test]
fn a_coding_goal_is_also_shown_the_search_and_the_multi_file_patch() {
    let h = real_host();
    for goal in ["fix the failing test in rust_probe", "refactor the parser module", "run cargo test and tell me what fails"] {
        let names: Vec<String> = tools_for_goal(&h, goal).into_iter().map(|t| t.name).collect();
        for must in ["fs.grep", "fs.patch"] {
            assert!(names.iter().any(|n| n == must), "{goal}: {must} is not shown: {names:?}");
        }
        assert!(names.len() <= MAX_HOST_TOOLS, "{names:?}");
    }
    // A goal about plain files is not given more than it asked for: the files pack is shown in the same order.
    let names: Vec<String> = tools_for_goal(&h, "read the file notes.txt").into_iter().map(|t| t.name).collect();
    assert!(names.iter().any(|n| n == "fs.read") && names.len() <= MAX_HOST_TOOLS);
}

#[test]
fn git_comes_with_the_two_packs_when_a_repository_is_named_and_not_otherwise() {
    let h = real_host();
    let with_git: Vec<String> = tools_for_goal(&h, "commit the fix in the repo and run cargo test").into_iter().map(|t| t.name).collect();
    assert!(with_git.iter().any(|n| n == "git.status") && with_git.iter().any(|n| n == "git.commit"), "{with_git:?}");
    assert!(with_git.iter().any(|n| n == "fs.edit") && with_git.iter().any(|n| n == "term.run"), "{with_git:?}");
    let without: Vec<String> = tools_for_goal(&h, "fix the failing test in rust_probe").into_iter().map(|t| t.name).collect();
    // A coding run is always shown the two git tools that only look (`git.status`, `git.diff`: they are in `PINNED_TOOLS`, so it can see what it changed); the tools that change a repository come
    // only with a goal that names one. (Until card OA5-01 this said "no git tool at all", which the pinning had already made untrue.)
    assert!(without.iter().any(|n| n == "git.status") && without.iter().any(|n| n == "git.diff"), "{without:?}");
    assert!(!without.iter().any(|n| matches!(n.as_str(), "git.commit" | "git.add" | "git.push" | "git.merge" | "git.pull")), "{without:?}");
}

/// Card OA5-03 (CX-17): the outline tool is not listed while its switch is off, and a coding goal is shown it, pinned, when it is on.
#[test]
fn a_coding_goal_is_shown_the_outline_tool_when_it_is_on_offer_and_not_otherwise() {
    use aqua_native_tools::fs::{FsConfig, FsProvider, PathGuard, Trash};
    let dir = std::env::temp_dir().join(format!("aqua_agent_tools_outline_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    let guard = Arc::new(PathGuard::new(FsConfig::with_workspace(dir.join("ws").to_string_lossy().to_string()), vec![dir.join("aqua")]).unwrap());
    let trash = Arc::new(Trash::new(dir.join("aqua").join("trash")).unwrap());
    let on = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let switch = on.clone();
    let h = ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(dir.join("aqua").join("audit.jsonl"))));
    h.register(Arc::new(FsProvider::new(guard, trash).with_outline(Arc::new(move || switch.load(Ordering::SeqCst))))).unwrap();
    let goal = "fix the failing test in src/lib.rs and run cargo test";
    assert!(!shown_names(&h, goal).iter().any(|n| n == "fs.outline"), "off: not listed, not shown");
    assert!(!h.manifests().iter().any(|m| m.name == "fs.outline"));
    on.store(true, Ordering::SeqCst);
    assert!(h.manifests().iter().any(|m| m.name == "fs.outline"));
    let shown = shown_names(&h, goal);
    assert!(shown.iter().any(|n| n == "fs.outline") && shown.len() <= MAX_HOST_TOOLS, "{shown:?}");
    // Its class is a read: it is shown with the read tools of the files pack.
    assert_eq!(pack_of("fs.outline"), Some("files"));
    assert!(is_read_only(Some(&h), "fs.outline"));
}

/// Card OA6-02 (ED-07): the line-edit tool is not listed while its switch is off, the list a coding goal is shown is then exactly what it is for a host that has no such switch at all, and
/// with the switch on a coding goal is shown the tool, pinned, in the pack of the file tools.
#[test]
fn a_coding_goal_is_shown_the_line_edit_tool_when_it_is_on_offer_and_not_otherwise() {
    use aqua_native_tools::fs::{FsConfig, FsProvider, PathGuard, Trash};
    let dir = std::env::temp_dir().join(format!("aqua_agent_tools_lines_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    let guard = Arc::new(PathGuard::new(FsConfig::with_workspace(dir.join("ws").to_string_lossy().to_string()), vec![dir.join("aqua")]).unwrap());
    let trash = Arc::new(Trash::new(dir.join("aqua").join("trash")).unwrap());
    let host = |name: &str, provider: FsProvider| {
        let h = ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(dir.join("aqua").join(name))));
        h.register(Arc::new(provider)).unwrap();
        h
    };
    let on = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let switch = on.clone();
    let with_switch = host("with.jsonl", FsProvider::new(guard.clone(), trash.clone()).with_edit_lines(Arc::new(move || switch.load(Ordering::SeqCst))));
    let without = host("without.jsonl", FsProvider::new(guard, trash));
    for goal in ["fix the failing test in src/lib.rs and run cargo test", "rename util.rs to helpers.rs and fix the imports", "read the file notes.txt in my folder", "what is in my workspace folder"] {
        assert_eq!(shown_names(&with_switch, goal), shown_names(&without, goal), "off: what is shown is exactly what a host without the tool shows: {goal}");
        assert!(!shown_names(&with_switch, goal).iter().any(|n| n == "fs.edit_lines"), "{goal}");
    }
    assert!(!with_switch.manifests().iter().any(|m| m.name == "fs.edit_lines") && !without.manifests().iter().any(|m| m.name == "fs.edit_lines"));
    assert_eq!(with_switch.manifests().len(), without.manifests().len());
    on.store(true, Ordering::SeqCst);
    assert!(with_switch.manifests().iter().any(|m| m.name == "fs.edit_lines"));
    let goal = "fix the failing test in src/lib.rs and run cargo test";
    let shown = shown_names(&with_switch, goal);
    assert!(shown.iter().any(|n| n == "fs.edit_lines") && shown.iter().any(|n| n == "fs.edit") && shown.len() <= MAX_HOST_TOOLS, "{shown:?}");
    assert_eq!(pack_of("fs.edit_lines"), Some("files"));
    assert!(!is_read_only(Some(&with_switch), "fs.edit_lines") && can_change_something(Some(&with_switch), "fs.edit_lines"), "it is an edit: a plan run may not call it");
    // The tool the model sees says what the call is: its line in the prompt has the arguments in the order they matter.
    let described = tools_for_goal(&with_switch, goal).into_iter().find(|t| t.name == "fs.edit_lines").unwrap().description;
    assert!(described.starts_with("(path, start, end, new") && described.contains("expect_first_line"), "{described}");
    on.store(false, Ordering::SeqCst);
    assert!(!shown_names(&with_switch, goal).iter().any(|n| n == "fs.edit_lines"), "off again: gone again");
}

/// Card OA4-05: `check.run` and `project.profile` are shown to a coding goal, pinned, when the host has them (they exist only with `AQUA_CHECK_TOOLS=on`), and no goal is routed to their pack by its words.
#[test]
fn a_coding_goal_is_shown_the_check_tools_when_they_are_on_offer_and_not_otherwise() {
    use aqua_native_tools::check::CheckProvider;
    use aqua_native_tools::fs::{FsConfig, FsProvider, PathGuard, Trash};
    use aqua_native_tools::term::{TermConfig, TermProvider};
    let dir = std::env::temp_dir().join(format!("aqua_agent_tools_check_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    let guard = Arc::new(PathGuard::new(FsConfig::with_workspace(dir.join("ws").to_string_lossy().to_string()), vec![dir.join("aqua")]).unwrap());
    let trash = Arc::new(Trash::new(dir.join("aqua").join("trash")).unwrap());
    let term = TermProvider::new(guard.clone(), TermConfig::default());
    let h = ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(dir.join("aqua").join("audit.jsonl"))));
    h.register(Arc::new(FsProvider::new(guard.clone(), trash))).unwrap();
    h.register(Arc::new(term.clone())).unwrap();
    let goal = "fix the failing test in src/lib.rs and run cargo test";
    let before = shown_names(&h, goal);
    assert!(!before.iter().any(|n| n == "check.run" || n == "project.profile"), "{before:?}");
    h.register(Arc::new(CheckProvider::new(guard, term, None))).unwrap();
    let shown = shown_names(&h, goal);
    assert!(shown.iter().any(|n| n == "check.run") && shown.iter().any(|n| n == "project.profile"), "{shown:?}");
    assert!(shown.len() <= MAX_HOST_TOOLS, "{} tools: {shown:?}", shown.len());
    for must in ["fs.edit", "fs.read", "term.run"] {
        assert!(shown.iter().any(|n| n == must), "{must} is still shown: {shown:?}");
    }
    // The pack has no words: no goal is routed to it, so with the tools there or not the routing of every other goal is what it was.
    assert_eq!((pack_of("check.run"), pack_of("project.profile")), (Some("check"), Some("check")));
    for g in ["check the weather", "run the tests of my notes app", "what is a build", "project plan for next week", goal] {
        assert!(!route(g).contains(&"check"), "{g}: {:?}", route(g));
    }
    // A goal that is no coding goal is shown none of them.
    assert!(!shown_names(&h, "what is the weather today").iter().any(|n| n == "check.run"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Card OA12-02: `code.symbols` and `code.outline` are shown to a coding goal, pinned, when the host lists them (they exist only with `AQUA_CODE_TOOLS=on`), and no goal is routed to their pack by
/// its words. They are the last of the tools that are pinned when on offer: with the outline of the file tools and the check tools on as well the list is full at twelve, and it is they that give way.
#[test]
fn a_coding_goal_is_shown_the_code_tools_when_they_are_on_offer_and_not_otherwise() {
    use aqua_native_tools::check::CheckProvider;
    use aqua_native_tools::code::provider::CodeProvider;
    use aqua_native_tools::fs::{FsConfig, FsProvider, PathGuard, Trash};
    use aqua_native_tools::term::{TermConfig, TermProvider};
    let dir = std::env::temp_dir().join(format!("aqua_agent_tools_code_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    let guard = Arc::new(PathGuard::new(FsConfig::with_workspace(dir.join("ws").to_string_lossy().to_string()), vec![dir.join("aqua")]).unwrap());
    let host = |name: &str, with_others: bool, on: Arc<std::sync::atomic::AtomicBool>| {
        let trash = Arc::new(Trash::new(dir.join("aqua").join(format!("trash_{name}"))).unwrap());
        let term = TermProvider::new(guard.clone(), TermConfig::default());
        let h = ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(dir.join("aqua").join(format!("audit_{name}.jsonl")))));
        let fs = FsProvider::new(guard.clone(), trash);
        h.register(Arc::new(if with_others { fs.with_outline(Arc::new(|| true)) } else { fs })).unwrap();
        h.register(Arc::new(term.clone())).unwrap();
        if with_others {
            h.register(Arc::new(CheckProvider::new(guard.clone(), term, None))).unwrap();
        }
        h.register(Arc::new(CodeProvider::new(guard.clone()).with_switch(Arc::new(move || on.load(Ordering::SeqCst))))).unwrap();
        h
    };
    let goal = "fix the failing test in src/lib.rs and run cargo test";
    let is_code = |n: &String| n.starts_with("code.");
    let on = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let h = host("alone", false, on.clone());
    assert!(!shown_names(&h, goal).iter().any(is_code), "off: not shown");
    assert!(!h.manifests().iter().any(|m| m.name.starts_with("code.")), "off: not listed");
    on.store(true, Ordering::SeqCst);
    let shown = shown_names(&h, goal);
    assert!(shown.iter().any(|n| n == "code.symbols") && shown.iter().any(|n| n == "code.outline"), "{shown:?}");
    // (Card OA12-03: `code.refs`, `code.deps` and `code.map` are pinned the same way, after the two; here the provider has one switch and the map follows it.)
    assert!(shown.iter().any(|n| n == "code.refs"), "{shown:?}");
    assert!(shown.len() <= MAX_HOST_TOOLS, "{} tools: {shown:?}", shown.len());
    for must in ["fs.edit", "fs.read", "fs.grep", "fs.patch", "term.run"] {
        assert!(shown.iter().any(|n| n == must), "{must} is still shown: {shown:?}");
    }
    // The two are reads, shown with the read tools; their pack has no words, so no goal is routed to it and the routing of every other goal is what it was.
    assert_eq!((pack_of("code.symbols"), pack_of("code.outline")), (Some("code"), Some("code")));
    assert!(["code.symbols", "code.outline", "code.deps", "code.refs", "code.map"].iter().all(|t| is_read_only(Some(&h), t)));
    assert!(["code.deps", "code.refs", "code.map"].iter().all(|t| pack_of(t) == Some("code")), "the new tools are in the pack of the others, which has no words");
    for g in ["find the code that parses the config", "where is the function defined", "show the outline of my notes", "symbols of the periodic table", goal] {
        assert!(!route(g).contains(&"code"), "{g}: {:?}", route(g));
    }
    // A goal that is no coding goal is shown none of them.
    assert!(!shown_names(&h, "what is the weather today").iter().any(is_code));
    // With the outline of the file tools and the check tools on as well, the list is full and stays at twelve: what the other switches added is kept, and the code tools are the ones cut.
    let everything = host("everything", true, Arc::new(std::sync::atomic::AtomicBool::new(true)));
    let shown = shown_names(&everything, goal);
    assert!(shown.len() <= MAX_HOST_TOOLS, "{} tools: {shown:?}", shown.len());
    for must in ["fs.edit", "fs.read", "fs.grep", "fs.patch", "term.run", "fs.outline", "check.run", "project.profile"] {
        assert!(shown.iter().any(|n| n == must), "{must} is still shown: {shown:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Card OA5-01 (TL-06): eleven things a coding run has to be able to do, each completable from the tools it is shown, with no `term.run exe=git` and no tool the list cut.
#[test]
fn coding_workflows_are_reachable() {
    let h = wide_host();
    let table: [(&str, &[&str]); 11] = [
        ("fix the failing test in rust_probe", &["fs.read", "fs.edit", "term.run"]),
        ("find where parse_line is used, fix the bug in it and run cargo test", &["fs.grep", "fs.read", "fs.edit", "term.run"]),
        ("create the file tests/new_case.rs with a test for parse_line and run cargo test", &["fs.write", "term.run"]),
        ("commit the fix in the repo and run cargo test", &["git.status", "git.diff", "git.commit", "fs.edit", "term.run"]),
        ("show me the diff of my changes in the repo", &["git.diff", "git.status"]),
        ("delete the unused file src/old.rs and run cargo build", &["fs.delete", "term.run"]),
        ("rename util.rs to helpers.rs and fix the imports", &["fs.move", "fs.edit"]),
        ("read the file build.log and tell me why cargo failed", &["fs.read", "fs.grep"]),
        ("list the files in the src folder", &["fs.list", "fs.find"]),
        ("fix the bug in src/a.rs and src/b.rs", &["fs.patch", "fs.edit"]),
        ("stage src/a.rs and commit it", &["git.add", "git.commit"]),
    ];
    for (goal, must) in table {
        let names = shown_names(&h, goal);
        for m in must {
            assert!(names.iter().any(|n| n == m), "{goal}: {m} is not shown: {names:?}");
        }
        assert!(names.len() <= MAX_HOST_TOOLS, "{goal}: {names:?}");
        let mut once = names.clone();
        once.sort();
        once.dedup();
        assert_eq!(once.len(), names.len(), "{goal}: a tool twice in {names:?}");
    }
}

/// The goal that failed at baseline for want of this: a commit goal is shown `git.commit`, and `git.commit` can stage by itself.
#[test]
fn commit_goal_shows_a_way_to_stage() {
    let h = wide_host();
    let tools = tools_for_goal(&h, "commit the fix in the repo and run cargo test");
    assert!(tools.iter().any(|t| t.name == "git.commit"), "git.commit is shown");
    assert_eq!(priority_of("git")[..3], ["git.status", "git.diff", "git.commit"], "the commit comes before the log");
    assert!(tools.len() <= MAX_HOST_TOOLS);
    // A goal that says it stages is also shown `git.add`, in a list that still fits.
    let staged = shown_names(&h, "stage src/a.rs and commit it");
    assert!(staged.iter().any(|n| n == "git.add") && staged.iter().any(|n| n == "git.commit"), "{staged:?}");
}

/// Card OA5-01 (TM-08): the tools of `coding_driver::DENY` are not in a coding list, and naming one does not bring it back; the same name for a goal that is not coding is the host's to decide on as before.
#[test]
fn a_denied_tool_cannot_be_reached_by_naming_it() {
    let h = wide_host();
    for goal in ["commit the fix in the repo and run cargo test", "fix the failing test in rust_probe", "push the fix to the remote repo, then run cargo test"] {
        let shown = tools_for_goal(&h, goal);
        for t in &shown {
            assert!(!crate::modules::coding_driver::denied(&t.name), "{goal}: {} is shown to a coding run", t.name);
        }
        for name in ["git.push", "git_push", "GIT.RESET", "ssh.exec"] {
            assert_eq!(canonical_hidden(name, &h, &shown, true), None, "{goal}: {name}");
        }
    }
    // A goal that only talks to git (not coding) is shown the git pack, and a denied name is its host's to decide on (it always asks).
    let git_only = tools_for_goal(&h, "show the git branches in the repo");
    assert_eq!(canonical_hidden("git.push", &h, &git_only, false).as_deref(), Some("git.push"));
    assert!(is_coding_goal("fix the failing test in rust_probe") && !is_coding_goal("what is the weather in Delhi"));
}

/// Card OA5-01 (TM-08, ED-12): `term.start` and `term.output` are shown together, whatever the goal and whatever room the list had.
#[test]
fn the_background_pair_is_shown_together_or_not_at_all() {
    let h = wide_host();
    for goal in [
        "fix the failing test in rust_probe",
        "commit the fix in the repo and run cargo test",
        "rename util.rs to helpers.rs and fix the imports",
        "delete the unused file src/old.rs and run cargo build",
        "run the dev server in the background, then fix the bug in src/a.rs",
        "build the project with cargo and read the output",
        "refactor the parser module and commit it in the repo",
        "rename c.rs to d.rs, delete the old file src/b.rs, stage both and commit the repo",
    ] {
        let names = shown_names(&h, goal);
        assert_eq!(names.iter().any(|n| n == "term.start"), names.iter().any(|n| n == "term.output"), "{goal}: {names:?}");
        assert!(names.len() <= MAX_HOST_TOOLS, "{goal}: {names:?}");
    }
}

/// Card OA5-01 (ED-12): a goal that renames is shown `fs.move`, one that deletes a file `fs.delete`, and a goal that only edits neither, in a list that still fits.
#[test]
fn a_rename_goal_is_shown_fs_move_and_a_plain_edit_goal_is_not() {
    let h = wide_host();
    let rename = shown_names(&h, "rename util.rs to helpers.rs and fix the imports");
    assert!(rename.iter().any(|n| n == "fs.move") && !rename.iter().any(|n| n == "fs.delete"), "{rename:?}");
    let delete = shown_names(&h, "delete the unused file src/old.rs and run cargo build");
    assert!(delete.iter().any(|n| n == "fs.delete") && !delete.iter().any(|n| n == "fs.move"), "{delete:?}");
    // Everything at once, with a git word: the list still fits, and what the goal asked for is in it.
    let all = shown_names(&h, "rename c.rs to d.rs, delete the old file src/b.rs, stage both and commit the repo");
    for must in ["fs.move", "fs.delete", "git.add", "git.commit", "fs.edit", "term.run"] {
        assert!(all.iter().any(|n| n == must), "{must} is not shown: {all:?}");
    }
    assert!(all.len() <= MAX_HOST_TOOLS, "{all:?}");
    for goal in ["fix the failing test in rust_probe", "implement caching in utils.py", "refactor the parser module", "remove the unused import from main.rs and run cargo test"] {
        let names = shown_names(&h, goal);
        assert!(!names.iter().any(|n| matches!(n.as_str(), "fs.move" | "fs.delete" | "git.add")), "{goal}: {names:?}");
    }
}

/// Card OA5-01 (WF-11, TM-08): with `AQUA_CODING_PACKS` on, a coding goal whose words call for one more pack is shown two tools of it, at most fourteen in all; with it off nothing changes.
#[test]
fn a_coding_goal_is_shown_two_tools_of_one_more_pack_only_with_the_switch_on() {
    let h = wide_host();
    let on = |goal: &str| crate::flags::with_overrides(&[("AQUA_CODING_PACKS", Some("on"))], || shown_names(&h, goal));
    let off = |goal: &str| crate::flags::with_overrides(&[("AQUA_CODING_PACKS", None)], || shown_names(&h, goal));
    let cases: [(&str, &[&str]); 4] = [
        ("write a migration for the users table in the project", &["db.schema", "db.query"]),
        ("fix the compose file in the project after checking the docker logs", &["docker.compose_logs", "docker.logs"]),
        ("fix the bug from the github issue in the project", &["forge.issues", "forge.prs"]),
        ("fix the bug in the project, then check the log for errors in the output", &["mon.log_tail", "mon.health"]),
    ];
    for (goal, extra) in cases {
        let before = off(goal);
        let after = on(goal);
        assert_eq!(before, shown_names(&h, goal), "{goal}: the switch is off unless it says on");
        for tool in extra {
            assert!(!before.iter().any(|n| n == tool), "{goal}: {tool} is shown with the switch off: {before:?}");
            assert!(after.iter().any(|n| n == tool), "{goal}: {tool} is not shown with the switch on: {after:?}");
        }
        assert_eq!(after.len(), before.len() + 2, "{goal}: {after:?}");
        assert!(after.len() <= MAX_CODING_HOST_TOOLS && MAX_CODING_HOST_TOOLS == 14);
        // Everything the list had stays.
        assert!(before.iter().all(|n| after.contains(n)), "{goal}");
    }
    // A goal whose words call for no such pack is shown what it was, and so is a goal that is not coding at all.
    for goal in ["fix the failing test in rust_probe", "rename util.rs to helpers.rs and fix the imports", "list the files in the src folder"] {
        assert_eq!(on(goal), off(goal), "{goal}");
    }
    // A tool of `DENY` is never the one added.
    let with_ssh = on("fix the bug in the project over ssh and look at the docker logs");
    assert!(with_ssh.iter().all(|n| !crate::modules::coding_driver::denied(n)), "{with_ssh:?}");
}

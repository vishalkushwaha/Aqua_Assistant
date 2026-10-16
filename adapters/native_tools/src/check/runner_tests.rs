//! The choice of the command of a check (card OA4-05, task WF-04): which command, which flags, which time, and what a call may not say. No program is run.

use super::*;
use crate::fs::{FsConfig, PathGuard};
use serde_json::json;

fn scratch(name: &str) -> PathBuf {
    let base = std::env::var_os("CARGO_TARGET_TMPDIR").or_else(|| std::env::var_os("CARGO_TARGET_DIR")).map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let d = base.join(format!("aqua_check_plan_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("ws")).unwrap();
    d
}

fn put(d: &Path, rel: &str, text: &str) {
    let p = d.join("ws").join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn guard(d: &Path) -> PathGuard {
    PathGuard::new(FsConfig::with_workspace(d.join("ws").to_string_lossy().to_string()), vec![]).unwrap()
}

fn planned(g: &PathGuard, args: Value) -> Plan {
    match plan(g, &args, None) {
        Ok(Planned::Run(p)) => *p,
        other => panic!("expected a command to run, got {other:?}"),
    }
}

fn refused(g: &PathGuard, args: Value) -> String {
    match plan(g, &args, None) {
        Err(e) => format!("{}: {}", e.code, e.message),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_rust_crate_gets_its_own_flags_for_each_kind_and_the_time_a_kind_deserves() {
    let d = scratch("rust");
    put(&d, "c/Cargo.toml", "[package]\nname = \"c\"\nversion = \"0.1.0\"\n");
    put(&d, "c/src/lib.rs", "");
    let g = guard(&d);
    let tests = planned(&g, json!({"kind": "tests", "path": "@ws/c"}));
    assert_eq!((tests.spec.exe.as_str(), tests.argv.as_slice()), ("cargo", &["test".to_string(), "--offline".to_string()][..]));
    assert_eq!((tests.timeout_s, tests.explicit_timeout, tests.background, tests.promote), (300, false, false, false), "a test run is stopped at its time");
    let filtered = planned(&g, json!({"kind": "tests", "path": "@ws/c/src/lib.rs", "filter": "parse::quotes", "package": "c"}));
    assert_eq!(filtered.argv, ["test", "--offline", "-p", "c", "parse::quotes"]);
    let build = planned(&g, json!({"kind": "build", "path": "@ws/c"}));
    assert_eq!(build.argv, ["build", "--offline", "--message-format=short"]);
    assert!(build.promote && !build.explicit_timeout, "a build with no time of its own is kept as a job at 300 s");
    let timed = planned(&g, json!({"kind": "build", "path": "@ws/c", "timeout_s": 500}));
    assert_eq!((timed.timeout_s, timed.promote), (300, false), "a time that is named is a limit: 300 at most, and it stops the run");
    let lint = planned(&g, json!({"kind": "lint", "path": "@ws/c"}));
    assert_eq!(lint.argv, ["clippy", "--offline", "--message-format=short"]);
    let background = planned(&g, json!({"kind": "tests", "path": "@ws/c", "background": true}));
    assert_eq!((background.timeout_s, background.term_tool()), (600, "term.start"));
    let long = planned(&g, json!({"kind": "tests", "path": "@ws/c", "background": true, "timeout_s": 5000}));
    assert_eq!(long.timeout_s, 3600);
    // The arguments handed to the terminal are the plain ones: the folder as an absolute path, no shell, nothing the model chose but the name.
    let args = filtered.term_args();
    assert_eq!((args["exe"].as_str(), args["args"][4].as_str(), args["max_output_kb"].as_u64()), (Some("cargo"), Some("parse::quotes"), Some(512)));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn what_a_model_may_say_for_a_filter_a_package_and_a_kind_is_a_name_and_nothing_else() {
    let d = scratch("names");
    put(&d, "c/Cargo.toml", "[package]\nname = \"c\"\nversion = \"0.1.0\"\n");
    let g = guard(&d);
    for bad in ["--help", "-p", "a b", "../x", "x/y", "x\\y", "$(calc)", "a;b", "a&b", "a|b", "`x`", "", "x".repeat(121).as_str()] {
        if bad.is_empty() {
            // An empty filter is no filter.
            let p = planned(&g, json!({"kind": "tests", "path": "@ws/c", "filter": bad}));
            assert_eq!(p.argv, ["test", "--offline"]);
            continue;
        }
        let why = refused(&g, json!({"kind": "tests", "path": "@ws/c", "filter": bad}));
        assert!(why.starts_with("INVALID_ARGUMENTS") && why.contains("plain name"), "{bad:?}: {why}");
        let why = refused(&g, json!({"kind": "tests", "path": "@ws/c", "package": bad}));
        assert!(why.starts_with("INVALID_ARGUMENTS") && why.contains("plain name"), "{bad:?}: {why}");
    }
    let why = refused(&g, json!({"kind": "deploy", "path": "@ws/c"}));
    assert!(why.contains("kind must be one of tests, build, lint, typecheck"), "{why}");
    for good in ["parse_quotes", "tests::adds", "test_app.T.test_ok", "test_x[1-2]", "Parse.Quotes-2"] {
        assert!(plan(&g, &json!({"kind": "tests", "path": "@ws/c", "filter": good}), None).is_ok(), "{good}");
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_python_node_and_go_project_get_the_forms_their_runners_take_and_refuse_the_ones_they_do_not() {
    let d = scratch("others");
    put(&d, "py/pyproject.toml", "[project]\nname = \"p\"\n[tool.pytest.ini_options]\nminversion = \"7\"\n");
    put(&d, "py/tests/test_a.py", "");
    put(&d, "plain_py/pyproject.toml", "[project]\nname = \"p\"\n");
    put(&d, "web/package.json", r#"{"scripts":{"test":"vitest run"}}"#);
    put(&d, "go/go.mod", "module x\n");
    let g = guard(&d);
    let all = planned(&g, json!({"kind": "tests", "path": "@ws/py"}));
    assert_eq!(all.argv, ["-m", "pytest", "-q", "--tb=short"]);
    let one = planned(&g, json!({"kind": "tests", "path": "@ws/py/tests/test_a.py", "filter": "adds"}));
    assert_eq!(one.argv, ["-m", "pytest", "-q", "--tb=short", "-k", "adds", "tests/test_a.py"], "a Python file is the one that is run, by its path from the project");
    let unit = planned(&g, json!({"kind": "tests", "path": "@ws/plain_py", "filter": "test_a.T.test_x"}));
    assert_eq!(unit.argv, ["-m", "unittest", "test_a.T.test_x"]);
    assert!(refused(&g, json!({"kind": "tests", "path": "@ws/py", "package": "x"})).contains("package is for a Cargo workspace"));
    let node = planned(&g, json!({"kind": "tests", "path": "@ws/web"}));
    assert_eq!((node.spec.exe.as_str(), node.argv.as_slice()), ("npm", &["test".to_string()][..]));
    assert!(refused(&g, json!({"kind": "tests", "path": "@ws/web", "filter": "adds"})).contains("not supported for `npm test`"));
    let go = planned(&g, json!({"kind": "tests", "path": "@ws/go", "filter": "TestParse"}));
    assert_eq!(go.argv, ["test", "-run", "TestParse", "./..."]);
    // A kind a project has no command for is a result and not an error, and says what the project does have.
    match plan(&g, &json!({"kind": "lint", "path": "@ws/web"}), None).unwrap() {
        Planned::NothingToRun { why, .. } => assert!(why.starts_with("this project has no lint command; it has: tests"), "{why}"),
        other => panic!("{other:?}"),
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_cargo_workspace_without_a_package_is_not_run_whole_and_a_path_inside_one_crate_is_the_crate() {
    let d = scratch("ws");
    put(&d, "w/Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\n");
    put(&d, "w/a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\n");
    put(&d, "w/a/src/lib.rs", "");
    put(&d, "w/b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\n");
    let g = guard(&d);
    let why = refused(&g, json!({"kind": "tests", "path": "@ws/w"}));
    assert!(why.contains("workspace of 2 crates (a, b)") && why.contains("One crate at a time"), "{why}");
    let named = planned(&g, json!({"kind": "tests", "path": "@ws/w", "package": "a"}));
    assert_eq!(named.argv, ["test", "--offline", "-p", "a"]);
    let inside = planned(&g, json!({"kind": "tests", "path": "@ws/w/a/src/lib.rs"}));
    assert_eq!(inside.argv, ["test", "--offline"]);
    assert!(inside.cwd.ends_with("a"), "{}", inside.cwd.display());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_path_outside_the_allowed_folders_and_a_folder_with_no_project_are_refused_before_anything_is_chosen() {
    let d = scratch("outside");
    put(&d, "plain/notes.txt", "");
    std::fs::create_dir_all(d.join("elsewhere")).unwrap();
    std::fs::write(d.join("elsewhere/Cargo.toml"), "[package]\nname = \"e\"\n").unwrap();
    let g = guard(&d);
    let why = refused(&g, json!({"kind": "tests", "path": "@ws/plain"}));
    assert!(why.starts_with("NOT_FOUND") && why.contains("no project file"), "{why}");
    let why = refused(&g, json!({"kind": "tests", "path": d.join("elsewhere").to_string_lossy()}));
    assert!(why.starts_with("PERMISSION_DENIED"), "{why}");
    let why = refused(&g, json!({"kind": "tests", "path": "@ws/missing"}));
    assert!(why.starts_with("NOT_FOUND") || why.starts_with("PERMISSION_DENIED"), "{why}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_snippet_is_read_through_the_guard_and_a_secret_or_a_file_outside_is_never_shown() {
    let d = scratch("snippet");
    put(&d, "c/src/lib.rs", "line one\nline two\nline three\nline four\nline five\nline six\nline seven\nline eight\n");
    put(&d, "c/.env", "KEY=1\nKEY=2\nKEY=3\n");
    put(&d, "c/secrets.pem", "-----BEGIN-----\nx\ny\n");
    std::fs::write(d.join("outside.rs"), "fn a() {}\n").unwrap();
    let g = guard(&d);
    let cwd = d.join("ws/c");
    let at = |file: &str, line: u32| Failure { id: "t".into(), file: Some(file.into()), line: Some(line), col: None, message: String::new() };
    let s = snippet_of(&g, &cwd, &cwd, &at("src/lib.rs", 4)).expect("a file of the project");
    assert_eq!((s.line, s.lines.first().map(|l| l.0), s.lines.last().map(|l| l.0)), (4, Some(1), Some(7)));
    assert_eq!(s.file, "src/lib.rs");
    let near_start = snippet_of(&g, &cwd, &cwd, &at("src/lib.rs", 1)).unwrap();
    assert_eq!(near_start.lines.first().map(|l| l.0), Some(1));
    assert!(snippet_of(&g, &cwd, &cwd, &at("src/lib.rs", 99)).is_none(), "a line the file does not have");
    assert!(snippet_of(&g, &cwd, &cwd, &at(".env", 2)).is_none(), "a file with a secret's name is not shown");
    assert!(snippet_of(&g, &cwd, &cwd, &at("secrets.pem", 2)).is_none());
    assert!(snippet_of(&g, &cwd, &cwd, &at(&d.join("outside.rs").to_string_lossy(), 1)).is_none(), "a file outside the allowed folders is not shown");
    assert!(snippet_of(&g, &cwd, &cwd, &Failure { id: "t".into(), file: None, line: None, col: None, message: String::new() }).is_none());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_path_is_shown_from_the_folder_the_command_ran_in() {
    let cwd = Path::new("C:/work/proj");
    assert_eq!(relative("C:\\work\\proj\\src\\lib.rs", cwd), "src/lib.rs");
    assert_eq!(relative("c:/WORK/proj/src/lib.rs", cwd), "src/lib.rs", "the case of a drive and a folder does not matter on Windows");
    assert_eq!(relative("src/lib.rs", cwd), "src/lib.rs");
    assert_eq!(relative("C:/work/projother/x.rs", cwd), "C:/work/projother/x.rs", "a folder that merely starts with the same letters is another folder");
    assert_eq!(relative("/usr/lib/x.rs", cwd), "/usr/lib/x.rs");
}

use super::model::*;
use super::run::exec::*;
use super::run::manager::FlowRunner;
use super::run::model::*;
use super::run::store::RunStore;
use super::sheet::book::{self, Book, Sheet, Val};
use super::sheet::ops::{from_val, to_val};
use super::sheet_exec::SheetNodeExecutor;
use super::store::FlowStore;
use super::supervisor::{classify, ErrorClass};
use aqua_native_tools::fs::{Access, FsConfig, PathGuard, Root};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("aqua_sheet_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn guard(root: &Path) -> Arc<PathGuard> {
    Arc::new(PathGuard::new(FsConfig::with_workspace(root.to_string_lossy().to_string()), vec![]).unwrap())
}

fn step(ex: &Arc<SheetNodeExecutor>, cfg: Value) -> Result<NodeResult, NodeError> {
    let node: Node = serde_json::from_value(json!({ "id": "sheet", "kind": "excel", "name": "Spreadsheet", "config": cfg })).unwrap();
    let cancel = AtomicBool::new(false);
    ex.execute(&NodeCtx { run_id: 1, node: &node, config: node.config.clone(), cancel: &cancel, timezone: "" })
}

fn append(ex: &Arc<SheetNodeExecutor>, path: &str, cols: Value) -> Result<NodeResult, NodeError> {
    step(ex, json!({ "operation": "append_row", "path": path, "sheet": "Posts", "columns": cols }))
}

fn read(ex: &Arc<SheetNodeExecutor>, path: &str) -> NodeResult {
    step(ex, json!({ "operation": "read_rows", "path": path, "sheet": "Posts" })).unwrap()
}

// ---- values ------------------------------------------------------------------------------------------------------------------------

#[test]
fn a_value_becomes_a_number_only_when_it_plainly_is_one_and_text_never_becomes_a_formula() {
    assert_eq!(to_val(&json!("42")), Val::Num(42.0));
    assert_eq!(to_val(&json!("-3.5")), Val::Num(-3.5));
    assert_eq!(to_val(&json!("0")), Val::Num(0.0));
    for text in ["007", "+91 98765 43210", "1e5", "12,5", "2026-10-01", "12345678901234567890", "1-2", "=SUM(A1:A9)", "@cmd", "-cmd"] {
        assert_eq!(to_val(&json!(text)), Val::Text(text.into()), "{text} stays text");
    }
    assert_eq!(to_val(&json!(7)), Val::Num(7.0));
    assert_eq!(to_val(&json!(true)), Val::Bool(true));
    assert_eq!(to_val(&json!(null)), Val::Empty);
    assert_eq!(to_val(&json!("")), Val::Empty);
    assert_eq!(from_val(&Val::Num(3.0)), json!(3));
    assert_eq!(from_val(&Val::Num(2.5)), json!(2.5));
    assert_eq!(super::sheet::book::excel_date_text(46_000.0), "2025-12-09");
    assert_eq!(super::sheet::book::excel_date_text(46_000.5), "2025-12-09 12:00");
}

// ---- files ------------------------------------------------------------------------------------------------------------------------

#[test]
fn a_workbook_survives_being_written_and_read_with_its_values_formulas_dates_and_other_sheets() {
    let d = dir("roundtrip");
    let path = d.join("book.xlsx");
    let book = Book {
        sheets: vec![
            Sheet { name: "Posts".into(), rows: vec![vec![Val::Text("Title".into()), Val::Text("Score".into()), Val::Text("Live".into()), Val::Text("When".into()), Val::Text("Double".into())], vec![Val::Text("First".into()), Val::Num(10.0), Val::Bool(true), Val::Date(46_000.5), Val::Formula("B2*2".into(), Some(Box::new(Val::Num(20.0))))], vec![Val::Text("=not a formula".into()), Val::Num(2.5), Val::Bool(false), Val::Empty, Val::Empty]] },
            Sheet { name: "Notes".into(), rows: vec![vec![Val::Text("Note".into())], vec![Val::Text("kept".into())]] },
        ],
        made_by_aqua: true,
    };
    book::save(&book, &path).unwrap();
    assert!(!d.join(".book.xlsx.aqua-tmp").exists(), "no temporary file is left");
    let back = book::load(&path).unwrap();
    assert!(back.made_by_aqua, "it says it was made by Aqua");
    assert_eq!(back.sheets.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["Posts", "Notes"]);
    let posts = &back.sheets[0].rows;
    assert_eq!(posts[1][0], Val::Text("First".into()));
    assert_eq!(posts[1][1], Val::Num(10.0));
    assert_eq!(posts[1][2], Val::Bool(true));
    assert_eq!(posts[1][3].show(), "2025-12-09 12:00", "a date stays a date");
    assert!(matches!(&posts[1][4], Val::Formula(f, _) if f == "B2*2"), "a formula stays a formula: {:?}", posts[1][4]);
    assert_eq!(posts[2][0], Val::Text("=not a formula".into()), "text that looks like a formula stays text");
    assert_eq!(back.sheets[1].rows[1][0], Val::Text("kept".into()));
    assert!(book::would_lose(&path).unwrap().is_empty(), "a workbook Aqua wrote has nothing a rewrite would lose");
}

#[test]
fn csv_files_round_trip_awkward_text_and_never_carry_a_formula_into_a_spreadsheet_program() {
    let d = dir("csv");
    let path = d.join("data.csv");
    let awkward = "He said \"hi\", then\nleft";
    let ex = SheetNodeExecutor::new(guard(&d));
    step(&ex, json!({ "operation": "append_row", "path": "data.csv", "columns": { "Name": awkward, "Amount": "12.5" } })).unwrap();
    step(&ex, json!({ "operation": "append_row", "path": "data.csv", "columns": { "Name": "=HYPERLINK(\"http://x\")", "Amount": "3" } })).unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("'=HYPERLINK"), "a leading = is neutralised in a CSV: {raw}");
    let r = step(&ex, json!({ "operation": "read_rows", "path": "data.csv" })).unwrap();
    assert_eq!(r.outputs["rows"][0]["Name"], awkward, "quotes, commas and line breaks survive");
    assert_eq!(r.outputs["row_count"], 2);
}

// ---- adding, changing, reading ---------------------------------------------------------------------------------------------------------

#[test]
fn adding_rows_makes_the_headers_once_finds_columns_by_name_and_never_invents_one() {
    let d = dir("append");
    let ex = SheetNodeExecutor::new(guard(&d));
    let r = append(&ex, "posts.xlsx", json!({ "Platform": "LinkedIn", "Text": "Hello", "Likes": "12" })).unwrap();
    assert_eq!((r.outputs["row_count"].clone(), r.outputs["rows"][0]["Platform"].clone()), (json!(1), json!("LinkedIn")));
    // Different order and case: the same columns.
    append(&ex, "posts.xlsx", json!({ "likes": 3, "PLATFORM": "X", "Text": "Second" })).unwrap();
    let rows = read(&ex, "posts.xlsx");
    assert_eq!(rows.outputs["row_count"], 2);
    assert_eq!(rows.outputs["rows"][1]["Platform"], "X");
    assert_eq!(rows.outputs["rows"][0]["Likes"], 12, "\"12\" was stored as a number");
    // A column the sheet does not have is an error that lists the ones it has.
    let e = append(&ex, "posts.xlsx", json!({ "Platform": "X", "Sentiment": "good" })).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("no column called \"Sentiment\"") && m.contains("Platform, Text, Likes")), "{e:?}");
    assert_eq!(classify(e.message()), ErrorClass::InvalidInput);
    assert_eq!(read(&ex, "posts.xlsx").outputs["row_count"], 2, "nothing was added");
    for bad in [json!({}), json!("not json")] {
        assert!(append(&ex, "posts.xlsx", bad).is_err());
    }
    // Text with a formula in it is stored as text and comes back as text.
    append(&ex, "posts.xlsx", json!({ "Text": "=1+1" })).unwrap();
    assert_eq!(read(&ex, "posts.xlsx").outputs["rows"][2]["Text"], "=1+1");
}

#[test]
fn a_row_is_changed_only_when_exactly_one_row_matches() {
    let d = dir("update");
    let ex = SheetNodeExecutor::new(guard(&d));
    for (id, status) in [("a1", "draft"), ("b2", "draft"), ("b2", "draft")] {
        append(&ex, "posts.xlsx", json!({ "Id": id, "Status": status })).unwrap();
    }
    let upd = |key: &str, cols: Value| step(&ex, json!({ "operation": "update_row", "path": "posts.xlsx", "sheet": "Posts", "key_column": "id", "key": key, "columns": cols }));
    let r = upd("a1", json!({ "Status": "published" })).unwrap();
    assert_eq!(r.outputs["rows"][0]["Status"], "published");
    let rows = read(&ex, "posts.xlsx").outputs["rows"].clone();
    assert_eq!((rows[0]["Status"].clone(), rows[1]["Status"].clone()), (json!("published"), json!("draft")), "only that row changed");
    let e = upd("zzz", json!({ "Status": "x" })).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("no row has")), "{e:?}");
    let e = upd("b2", json!({ "Status": "x" })).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("2 rows have") && m.contains("nothing was changed")), "{e:?}");
    let e = step(&ex, json!({ "operation": "update_row", "path": "posts.xlsx", "sheet": "Posts", "key_column": "nope", "key": "a1", "columns": { "Status": "x" } })).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("no column called \"nope\"")));
    let unchanged = read(&ex, "posts.xlsx").outputs["rows"].clone();
    assert_eq!(unchanged[1]["Status"], "draft");
}

#[test]
fn reading_skips_blank_rows_honours_the_limit_and_says_when_a_sheet_or_file_is_missing() {
    let d = dir("read");
    let ex = SheetNodeExecutor::new(guard(&d));
    let rows: Vec<Value> = (1..=12).map(|i| json!({ "N": i, "Word": format!("w{i}") })).collect();
    step(&ex, json!({ "operation": "export", "path": "words.xlsx", "sheet": "Words", "rows": rows })).unwrap();
    let r = step(&ex, json!({ "operation": "read_rows", "path": "words.xlsx", "sheet": "Words", "limit": 5 })).unwrap();
    assert_eq!((r.outputs["rows"].as_array().unwrap().len(), r.outputs["row_count"].clone()), (5, json!(12)), "five returned, twelve there");
    assert_eq!(r.outputs["rows"][4]["Word"], "w5");
    let e = step(&ex, json!({ "operation": "read_rows", "path": "words.xlsx", "sheet": "Nope" })).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("no sheet called \"Nope\"") && m.contains("Words")), "{e:?}");
    let e = step(&ex, json!({ "operation": "read_rows", "path": "absent.xlsx" })).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("no such file")));
    assert_eq!(classify(e.message()), ErrorClass::MissingFile);
    // A workbook can also be built from a flow's own data and used by the next step.
    let out = step(&ex, json!({ "operation": "read_rows", "path": "words.xlsx", "limit": "3" })).unwrap();
    assert_eq!(out.outputs["rows"].as_array().unwrap().len(), 3, "a limit written as text is understood");
}

// ---- safety ---------------------------------------------------------------------------------------------------------------------------

/// A workbook made by "someone else", optionally with merged cells.
fn foreign(path: &Path, merged: bool) {
    let mut wb = rust_xlsxwriter::Workbook::new();
    wb.set_properties(&rust_xlsxwriter::DocProperties::new().set_author("Someone Else"));
    let ws = wb.add_worksheet();
    ws.set_name("Posts").unwrap();
    ws.write_string(0, 0, "Title").unwrap();
    ws.write_string(0, 1, "Score").unwrap();
    ws.write_string(1, 0, "Existing").unwrap();
    ws.write_number(1, 1, 5.0).unwrap();
    if merged {
        ws.merge_range(3, 0, 3, 1, "A merged banner", &rust_xlsxwriter::Format::new().set_bold()).unwrap();
    }
    wb.save(path).unwrap();
}

#[test]
fn a_workbook_aqua_did_not_make_is_not_rewritten_unless_allowed_and_never_when_that_would_lose_something() {
    let d = dir("foreign");
    let ex = SheetNodeExecutor::new(guard(&d));
    foreign(&d.join("theirs.xlsx"), false);
    let add = |flag: bool| step(&ex, json!({ "operation": "append_row", "path": "theirs.xlsx", "sheet": "Posts", "columns": { "Title": "New" }, "rewrite_existing": flag }));
    let e = add(false).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("not made by Aqua") && m.contains("formatting") && m.contains("Allow rewriting")), "{e:?}");
    assert_eq!(classify(e.message()), ErrorClass::PermissionError);
    assert_eq!(read(&ex, "theirs.xlsx").outputs["row_count"], 1, "untouched");
    assert!(std::fs::read_dir(&d).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains("backup")), "no backup, because nothing was changed");

    let r = add(true).unwrap();
    assert_eq!(r.outputs["row_count"], 2);
    assert!(r.logs.iter().any(|l| l.contains("Kept a backup")), "{:?}", r.logs);
    let backups: Vec<String> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).filter(|n| n.contains("aqua-backup")).collect();
    assert_eq!(backups.len(), 1);
    let old = book::load(&d.join(&backups[0])).unwrap();
    assert_eq!(old.sheets[0].rows.len(), 2, "the backup is the file as it was");
    assert_eq!(read(&ex, "theirs.xlsx").outputs["rows"][0]["Title"], "Existing", "the person's own row is kept");

    // Merged cells would be lost: never rewritten, even when allowed.
    foreign(&d.join("merged.xlsx"), true);
    let e = step(&ex, json!({ "operation": "append_row", "path": "merged.xlsx", "sheet": "Posts", "columns": { "Title": "New" }, "rewrite_existing": true })).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("merged cells") && m.contains("will not change it")), "{e:?}");
    // Export over an existing file needs the same permission.
    let e = step(&ex, json!({ "operation": "export", "path": "theirs.xlsx", "rows": [{ "A": 1 }] })).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("already exists")), "{e:?}");
}

#[test]
fn only_the_newest_backups_are_kept_and_no_temporary_file_is_left() {
    let d = dir("backups");
    let path = d.join("log.csv");
    let mut b = Book { sheets: vec![Sheet { name: "Sheet1".into(), rows: vec![vec![Val::Text("A".into())]] }], made_by_aqua: true };
    book::save(&b, &path).unwrap();
    for i in 0..9 {
        b.sheets[0].rows.push(vec![Val::Text(format!("r{i}"))]);
        book::backup(&path, 1_800_000_000 + i * 10).unwrap();
        book::save(&b, &path).unwrap();
    }
    let names: Vec<String> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
    assert_eq!(names.iter().filter(|n| n.contains("aqua-backup")).count(), 5, "{names:?}");
    assert!(names.iter().all(|n| !n.ends_with(".aqua-tmp")), "{names:?}");
    assert!(names.iter().any(|n| n.contains("20270115") || n.contains("2027")), "the newest are the ones kept: {names:?}");
}

#[test]
fn a_step_can_only_reach_the_folders_aqua_may_use_and_only_write_where_it_may_without_asking() {
    let d = dir("guard");
    let outside = dir("guard_outside");
    let ex = SheetNodeExecutor::new(guard(&d));
    let elsewhere = outside.join("x.xlsx");
    for cfg in [json!({ "operation": "append_row", "path": elsewhere.to_string_lossy(), "columns": { "A": 1 } }), json!({ "operation": "read_rows", "path": elsewhere.to_string_lossy() }), json!({ "operation": "append_row", "path": "../escape.xlsx", "columns": { "A": 1 } })] {
        let e = step(&ex, cfg.clone()).unwrap_err();
        assert!(matches!(&e, NodeError::Fatal(m) if m.starts_with("not allowed")), "{cfg}: {e:?}");
        assert_eq!(classify(e.message()), ErrorClass::PermissionError);
    }
    assert!(!elsewhere.exists() && !d.parent().unwrap().join("escape.xlsx").exists());
    for (path, needle) in [("secrets.xlsx", "holds secrets"), ("notes.docx", "must be .xlsx or .csv"), ("run.bat", "can run code")] {
        let e = append(&ex, path, json!({ "A": 1 })).unwrap_err();
        assert!(matches!(&e, NodeError::Fatal(m) if m.contains(needle)), "{path}: {e:?}");
    }
    // Read-only and not-trusted folders cannot be written to (a flow cannot answer a prompt), but can be read.
    let ro = dir("guard_ro");
    foreign(&ro.join("data.xlsx"), false);
    let cfg = FsConfig { roots: vec![Root { alias: "ro".into(), path: ro.to_string_lossy().to_string(), access: Access::Ro, trusted: false }], ..FsConfig::with_workspace(ro.to_string_lossy().to_string()) };
    let ex_ro = SheetNodeExecutor::new(Arc::new(PathGuard::new(cfg, vec![]).unwrap()));
    assert_eq!(read(&ex_ro, "data.xlsx").outputs["row_count"], 1);
    let e = append(&ex_ro, "data.xlsx", json!({ "Title": "x" })).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.starts_with("not allowed") && m.contains("read-only")), "{e:?}");
    let untrusted = FsConfig { roots: vec![Root { alias: "u".into(), path: ro.to_string_lossy().to_string(), access: Access::Rw, trusted: false }], ..FsConfig::with_workspace(ro.to_string_lossy().to_string()) };
    let ex_u = SheetNodeExecutor::new(Arc::new(PathGuard::new(untrusted, vec![]).unwrap()));
    let e = append(&ex_u, "data.xlsx", json!({ "Title": "x" })).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("not set to allow writing without asking")), "{e:?}");
}

#[test]
fn many_steps_adding_to_one_file_at_once_lose_no_row() {
    let d = dir("concurrent");
    let ex = SheetNodeExecutor::new(guard(&d));
    append(&ex, "shared.xlsx", json!({ "Who": "seed", "N": 0 })).unwrap();
    let handles: Vec<_> = (1..=8)
        .map(|i| {
            let ex = ex.clone();
            std::thread::spawn(move || append(&ex, "shared.xlsx", json!({ "Who": format!("t{i}"), "N": i })).unwrap())
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let rows = read(&ex, "shared.xlsx");
    assert_eq!(rows.outputs["row_count"], 9, "every writer's row is there");
    let mut seen: Vec<i64> = rows.outputs["rows"].as_array().unwrap().iter().map(|r| r["N"].as_i64().unwrap()).collect();
    seen.sort();
    assert_eq!(seen, (0..=8).collect::<Vec<_>>());
}

// ---- inside a real run ---------------------------------------------------------------------------------------------------------------------

fn rig(name: &str, root: &Path) -> (Arc<FlowRunner>, Arc<RunStore>, Arc<FlowStore>) {
    let dbdir = std::env::temp_dir().join(format!("aqua_sheet_db_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dbdir);
    let path = dbdir.join("flows.db");
    let (flows, runs) = (Arc::new(FlowStore::new(&path)), Arc::new(RunStore::new(&path)));
    let runner = FlowRunner::with_clock(flows.clone(), runs.clone(), Arc::new(Executors::new().with(SheetNodeExecutor::new(guard(root)))), Arc::new(|_, c| !c.load(std::sync::atomic::Ordering::SeqCst)), Arc::new(|| 1_800_000_000));
    (runner, runs, flows)
}

fn save_flow() -> FlowDef {
    serde_json::from_value(json!({
        "name": "Save a post",
        "settings": { "approval": "never" },
        "variables": [{ "name": "platform", "label": "Platform", "kind": "text", "required": true, "default": "LinkedIn" }],
        "nodes": [
            { "id": "start", "kind": "trigger", "config": { "trigger_type": "manual" } },
            { "id": "save", "kind": "excel", "name": "Save to Excel", "config": { "operation": "append_row", "path": "posts.xlsx", "sheet": "Posts", "columns": { "Platform": "{{vars.platform}}", "Status": "published" } } },
            { "id": "read", "kind": "excel", "name": "Count them", "config": { "operation": "read_rows", "path": "posts.xlsx", "sheet": "Posts" } },
            { "id": "say", "kind": "notification", "config": { "channel": "in_app", "message": "rows now: {{nodes.read.row_count}}" } }
        ],
        "edges": [{ "from": "start", "to": "save" }, { "from": "save", "to": "read" }, { "from": "read", "to": "say" }]
    }))
    .unwrap()
}

#[test]
fn a_flow_saves_a_row_and_the_next_step_reads_it_back_and_a_dry_run_writes_nothing() {
    let d = dir("flow");
    let (runner, runs, flows) = rig("flow", &d);
    let id = flows.create("local", save_flow(), "user", "", 1_800_000_000).unwrap().id;
    let mut vars = Map::new();
    vars.insert("platform".into(), json!("Instagram"));
    let run = runner.start("local", id, vars, Value::Null, false, "manual").unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    let done = runs.get_run("local", run.id).unwrap().unwrap();
    assert_eq!(done.status, RunStatus::Succeeded, "{}", done.error);
    let events: Vec<String> = runs.events("local", run.id, 0, 100).unwrap().into_iter().map(|e| e.message).collect();
    assert!(events.iter().any(|m| m.contains("rows now: 1")), "{events:?}");
    let b = book::load(&d.join("posts.xlsx")).unwrap();
    assert_eq!(b.sheets[0].rows[1][0], Val::Text("Instagram".into()));

    let dry = runner.start("local", id, Map::new(), Value::Null, true, "manual").unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    assert_eq!(runs.get_run("local", dry.id).unwrap().unwrap().status, RunStatus::Succeeded);
    assert_eq!(book::load(&d.join("posts.xlsx")).unwrap().sheets[0].rows.len(), 2, "a dry run added nothing");
}

#[test]
fn a_row_that_was_added_is_never_added_again_when_a_run_is_picked_up_after_a_crash() {
    let d = dir("ledger");
    let (runner, runs, flows) = rig("ledger", &d);
    let id = flows.create("local", save_flow(), "user", "", 1_800_000_000).unwrap().id;
    let run = runner.start("local", id, Map::new(), Value::Null, false, "manual").unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    assert_eq!(book::load(&d.join("posts.xlsx")).unwrap().sheets[0].rows.len(), 2);
    // As after a restart: the run is re-driven and the save step is pending again.
    runs.set_status(run.id, RunStatus::Interrupted, "restart", 1_800_000_100).unwrap();
    let mut n = runs.node_run(run.id, "save").unwrap().unwrap();
    n.status = NodeStatus::Pending;
    runs.save_node(&n).unwrap();
    runner.resume("local", run.id).unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    assert_eq!(book::load(&d.join("posts.xlsx")).unwrap().sheets[0].rows.len(), 2, "the recorded save was reused, not done again");
    let done = runs.get_run("local", run.id).unwrap().unwrap();
    assert_eq!(done.status, RunStatus::Succeeded);
}

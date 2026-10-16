//! Card R7-12: the For each step in a real run. Nothing here needs a model: the steps that reach outside are the fake of `run_tests`, and the list comes from a
//! spreadsheet step that the fake answers with rows.

use super::*;

fn names(list: &[&str]) -> Value {
    Value::Array(list.iter().map(|n| json!({ "name": n })).collect())
}

/// What the spreadsheet step called "read" will answer with.
fn reads(r: &Rig, rows: Value) {
    r.fake.script("read", vec![Step::Ok(vec![("rows", rows)])]);
}

fn read_step() -> Value {
    n("read", "excel", json!({ "operation": "read_rows", "path": "people.xlsx" }))
}

fn each(items: &str) -> Value {
    n("each", "loop", json!({ "items": items }))
}

/// A web request for each row: the row's name and its number are in the address, so a test can see what each run was given.
fn get(on_error: &str) -> Value {
    let mut v = n("get", "http", json!({ "method": "GET", "url": "https://example.com/{{nodes.each.item.name}}?i={{nodes.each.index}}" }));
    v["on_error"] = json!(on_error);
    v
}

/// A web request for each item of a list of plain texts.
fn visit(on_error: &str) -> Value {
    let mut v = n("visit", "http", json!({ "method": "GET", "url": "https://example.com/{{nodes.each.item}}" }));
    v["on_error"] = json!(on_error);
    v
}

/// The addresses a step was asked for, in order.
fn urls(r: &Rig, id: &str) -> Vec<String> {
    r.fake.calls_to(id).iter().map(|c| c["url"].as_str().unwrap_or("").to_string()).collect()
}

/// start -> read (the rows) -> each, and by "done" a notice that says `after`; `inside` are the steps that the "each" exit leads to, with their connections.
fn per_row(r: &Rig, after: &str, inside: Vec<Value>, inside_edges: Vec<Value>) -> i64 {
    let mut nodes = vec![trigger(), read_step(), each("{{nodes.read.rows}}"), notify("after", after)];
    nodes.extend(inside);
    let mut edges = vec![json!(["start", "read"]), json!(["read", "each"]), json!(["each", "after", "done"])];
    edges.extend(inside_edges);
    r.flow(json!({ "name": "Per row", "nodes": nodes, "edges": edges }))
}

fn get_flow(r: &Rig, on_error: &str) -> i64 {
    per_row(r, "{{nodes.each.count}} done, the last was {{nodes.each.item.name}}", vec![get(on_error)], vec![json!(["each", "get", "each"])])
}

/// A post for each row (a step with an effect outside Aqua, so it is in the ledger), with approvals switched off.
fn send_flow(r: &Rig) -> i64 {
    r.flow(json!({ "name": "Send", "settings": { "approval": "never" }, "nodes": [trigger(), read_step(), each("{{nodes.read.rows}}"), n("send", "social_media", json!({ "platform": "linkedin", "action": "publish", "text": "{{nodes.each.item.name}}" })), notify("after", "{{nodes.each.count}} sent")],
        "edges": [["start", "read"], ["read", "each"], ["each", "send", "each"], ["each", "after", "done"]] }))
}

/// A second runner on the same database, as after Aqua is started again.
fn second_runner(r: &Rig) -> (Arc<FlowRunner>, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    let execs = Arc::new(Executors::new().with(fake.clone()));
    let sleep: Arc<crate::flows::run::engine::Sleeper> = Arc::new(|_secs, cancel| !cancel.load(Ordering::SeqCst));
    let clock = r.clock.clone();
    (FlowRunner::with_clock(r.flows.clone(), r.runs.clone(), execs, sleep, Arc::new(move || clock.load(Ordering::SeqCst))), fake)
}

// ---- the plain run -------------------------------------------------------------------------------------------------------

#[test]
fn a_for_each_step_runs_what_is_connected_to_each_once_for_every_item_and_then_goes_on_by_done() {
    let r = rig("loop_basic");
    reads(&r, names(&["ann", "bob", "cy"]));
    let f = get_flow(&r, "fail");
    let run = r.go_and_settle(f);
    assert_eq!(run.status, RunStatus::Succeeded, "{:?}", r.messages(run.id));
    // Each item in turn, with its own number.
    assert_eq!(urls(&r, "get"), ["https://example.com/ann?i=0", "https://example.com/bob?i=1", "https://example.com/cy?i=2"]);
    // Then "done": what follows can read how many there were and the last item.
    assert!(r.messages(run.id).iter().any(|m| m == "3 done, the last was cy"), "{:?}", r.messages(run.id));
    let each = r.node(run.id, "each");
    assert_eq!((each.status, each.taken.clone(), each.note.clone()), (NodeStatus::Succeeded, vec!["done".to_string()], "ran 3 items".to_string()));
    assert_eq!((each.outputs["count"].clone(), each.outputs["index"].clone(), each.outputs["number"].clone(), each.outputs["item"].clone()), (json!(3), json!(2), json!(3), json!({ "name": "cy" })));
    assert!(each.looping.is_none(), "a step that is done keeps no place in its list");
    // What the steps inside made for each item is kept, by the id of the step.
    let results = each.outputs["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|item| item["get"]["response"] == "get:response" && item["get"]["status"] == "get:status"), "{results:?}");
    assert_eq!((r.node(run.id, "get").status, r.node(run.id, "after").status), (NodeStatus::Succeeded, NodeStatus::Succeeded));
    // The log says which item it is on.
    let said: Vec<String> = r.events(run.id).into_iter().filter(|e| e.kind == "loop_item").map(|e| e.message).collect();
    assert_eq!(said, ["\"each\": item 1 of 3", "\"each\": item 2 of 3", "\"each\": item 3 of 3"]);
    assert!(r.messages(run.id).iter().any(|m| m == "\"each\": 3 items"));
    assert_eq!(r.runner.active_count(), 0);
}

#[test]
fn an_empty_list_goes_straight_to_done_and_what_is_inside_is_skipped() {
    let r = rig("loop_empty");
    reads(&r, json!([]));
    let f = per_row(&r, "{{nodes.each.count}} items", vec![get("fail")], vec![json!(["each", "get", "each"])]);
    let run = r.go_and_settle(f);
    assert_eq!(run.status, RunStatus::Succeeded, "{:?}", r.messages(run.id));
    assert_eq!(r.fake.count("get"), 0);
    assert_eq!((r.node(run.id, "get").status, r.node(run.id, "after").status), (NodeStatus::Skipped, NodeStatus::Succeeded));
    let each = r.node(run.id, "each");
    assert_eq!((each.outputs["count"].clone(), each.outputs["results"].clone(), each.outputs["item"].clone(), each.note.clone()), (json!(0), json!([]), Value::Null, "the list is empty, so the steps inside did not run".to_string()));
    assert!(r.messages(run.id).iter().any(|m| m == "0 items"));
    // Nothing at all is as good as an empty list; text with nothing in it too.
    for empty in [Value::Null, json!(""), json!("   \n ")] {
        let r = rig("loop_empty_kinds");
        reads(&r, empty.clone());
        let f = per_row(&r, "{{nodes.each.count}} items", vec![get("fail")], vec![json!(["each", "get", "each"])]);
        let run = r.go_and_settle(f);
        // A missing value is an error when it is looked up (that is the engine's old rule), an empty text is no items.
        if empty.is_null() {
            assert_eq!(run.status, RunStatus::Failed, "{:?}", r.messages(run.id));
        } else {
            assert_eq!((run.status, r.fake.count("get")), (RunStatus::Succeeded, 0), "{empty}: {:?}", r.messages(run.id));
        }
    }
}

#[test]
fn a_list_can_be_written_as_json_as_lines_as_a_line_of_commas_or_come_from_an_input_and_one_that_is_not_a_list_stops_the_step() {
    let cases: [(&str, Vec<&str>); 3] = [("[\"x\", \"y\"]", vec!["x", "y"]), ("one\ntwo", vec!["one", "two"]), ("a, b ,c", vec!["a", "b", "c"])];
    for (i, (items, want)) in cases.iter().enumerate() {
        let r = rig(&format!("loop_text_{i}"));
        let f = r.flow(json!({ "name": "Text", "nodes": [trigger(), each(items), visit("fail")], "edges": [["start", "each"], ["each", "visit", "each"]] }));
        let run = r.go_and_settle(f);
        assert_eq!(run.status, RunStatus::Succeeded, "{items:?}: {:?}", r.messages(run.id));
        let want: Vec<String> = want.iter().map(|w| format!("https://example.com/{w}")).collect();
        assert_eq!(urls(&r, "visit"), want, "{items:?}");
    }
    // From a flow input.
    let r = rig("loop_input");
    let f = r.flow(json!({ "name": "Input", "variables": [{ "name": "list", "label": "List", "kind": "text", "required": false, "default": "p,q" }], "nodes": [trigger(), each("{{vars.list}}"), visit("fail")], "edges": [["start", "each"], ["each", "visit", "each"]] }));
    let run = r.go_and_settle(f);
    assert_eq!((run.status, urls(&r, "visit")), (RunStatus::Succeeded, vec!["https://example.com/p".to_string(), "https://example.com/q".to_string()]));
    // A record or a number is not a list: the step stops, with the reason, before anything inside it runs.
    for (rows, why) in [(json!({ "a": 1 }), "single record"), (json!(5), "the list is 5, which is not a list")] {
        let r = rig("loop_not_a_list");
        reads(&r, rows);
        let f = get_flow(&r, "fail");
        let run = r.go_and_settle(f);
        assert_eq!(run.status, RunStatus::Failed);
        assert!(run.error.contains("\"each\" failed:") && run.error.contains(why), "{}", run.error);
        assert_eq!((r.fake.count("get"), r.node(run.id, "each").status), (0, NodeStatus::Failed));
    }
}

#[test]
fn results_of_every_item_can_be_read_after_the_loop_and_a_reference_into_them_passes_the_checker() {
    let r = rig("loop_results");
    reads(&r, names(&["ann", "bob", "cy"]));
    r.fake.script("write", vec![Step::Ok(vec![("text", json!("T0"))]), Step::Ok(vec![("text", json!("T1"))]), Step::Ok(vec![("text", json!("T2"))])]);
    let f = per_row(&r, "{{nodes.each.results[*].write.text | join: \" / \"}}", vec![text("write", "Write about {{nodes.each.item.name}}")], vec![json!(["each", "write", "each"])]);
    let run = r.go_and_settle(f);
    assert_eq!(run.status, RunStatus::Succeeded, "{:?}", r.messages(run.id));
    assert!(r.messages(run.id).iter().any(|m| m == "T0 / T1 / T2"), "{:?}", r.messages(run.id));
}

#[test]
fn a_condition_inside_a_for_each_takes_its_own_side_for_every_item() {
    let r = rig("loop_condition");
    reads(&r, names(&["ann", "bob", "cy"]));
    let side = |id: &str| n(id, "http", json!({ "method": "GET", "url": format!("https://example.com/{id}/{{{{nodes.each.item.name}}}}") }));
    let f = per_row(&r, "all done", vec![n("cond", "condition", json!({ "expression": "{{nodes.each.index}} == 1" })), side("special"), side("plain")], vec![json!(["each", "cond", "each"]), json!(["cond", "special", "true"]), json!(["cond", "plain", "false"])]);
    let run = r.go_and_settle(f);
    assert_eq!(run.status, RunStatus::Succeeded, "{:?}", r.messages(run.id));
    assert_eq!(urls(&r, "plain"), ["https://example.com/plain/ann", "https://example.com/plain/cy"]);
    assert_eq!(urls(&r, "special"), ["https://example.com/special/bob"]);
    // A side that was not taken made nothing for that item.
    let results = r.node(run.id, "each").outputs["results"].as_array().unwrap().clone();
    assert!(results[1].get("special").is_some() && results[1].get("plain").is_none() && results[0].get("plain").is_some() && results[0].get("special").is_none(), "{results:?}");
}

// ---- limits --------------------------------------------------------------------------------------------------------------

#[test]
fn a_list_over_a_limit_stops_the_step_before_anything_inside_it_runs_with_a_message_and_is_never_cut_short() {
    let r = rig("loop_limits");
    // One item too many.
    reads(&r, Value::Array((0..201).map(|i| json!({ "name": format!("n{i}") })).collect()));
    let f = get_flow(&r, "fail");
    let run = r.go_and_settle(f);
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.error.contains("\"each\" failed: the list has 201 items") && run.error.contains("at most 200") && run.error.contains("Nothing was run"), "{}", run.error);
    assert_eq!((r.fake.count("get"), r.node(run.id, "each").status, r.node(run.id, "after").status), (0, NodeStatus::Failed, NodeStatus::Skipped), "not one item was run");

    // Too many step runs: 101 items with 21 steps inside could run 2121.
    let r = rig("loop_step_runs");
    reads(&r, json!((0..101).collect::<Vec<i32>>()));
    let mut nodes = vec![trigger(), read_step(), each("{{nodes.read.rows}}"), notify("after", "done")];
    let mut edges = vec![json!(["start", "read"]), json!(["read", "each"]), json!(["each", "s0", "each"]), json!(["each", "after", "done"])];
    for i in 0..21 {
        nodes.push(notify(&format!("s{i}"), "inside"));
        if i > 0 {
            edges.push(json!([format!("s{}", i - 1), format!("s{i}")]));
        }
    }
    let f = r.flow(json!({ "name": "Too many runs", "nodes": nodes, "edges": edges }));
    let run = r.go_and_settle(f);
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.error.contains("101 items with 21 steps inside could run 2121 steps") && run.error.contains("at most 2000") && run.error.contains("Nothing was run"), "{}", run.error);
    assert!(r.events(run.id).iter().all(|e| e.level != "notify"), "no step inside ran");
}

// ---- a step inside fails -----------------------------------------------------------------------------------------------------

#[test]
fn a_step_inside_that_fails_and_lets_the_flow_carry_on_does_not_stop_the_list() {
    let r = rig("loop_continue");
    reads(&r, names(&["ann", "bob", "cy"]));
    r.fake.script("get", vec![Step::Ok(vec![("response", json!("r0"))]), Step::Fatal("site down")]);
    let f = get_flow(&r, "continue");
    let run = r.go_and_settle(f);
    assert_eq!(run.status, RunStatus::Succeeded, "{:?}", r.messages(run.id));
    assert_eq!(r.fake.count("get"), 3, "every item was tried");
    let results = r.node(run.id, "each").outputs["results"].as_array().unwrap().clone();
    assert_eq!(results[0]["get"]["response"], "r0");
    assert_eq!(results[1]["get"], json!({ "ok": false, "error": "site down" }), "the item that failed says so");
    assert_eq!(results[2]["get"]["response"], "get:response");
    assert!(r.messages(run.id).iter().any(|m| m.contains("\"get\" failed on item 2 of 3 and the flow carries on: site down")), "{:?}", r.messages(run.id));
}

#[test]
fn a_step_inside_that_fails_and_stops_the_run_says_which_item_and_can_be_tried_again_where_the_list_was() {
    let r = rig("loop_fail");
    reads(&r, names(&["ann", "bob", "cy"]));
    r.fake.script("get", vec![Step::Ok(vec![]), Step::Fatal("broke")]);
    let f = get_flow(&r, "fail");
    let run = r.go_and_settle(f);
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.error.contains("\"get\" failed on item 2 of 3: broke"), "{}", run.error);
    assert_eq!(r.fake.count("get"), 2, "the third item was not started");
    // The For each step keeps its place and is not shown as running in a run that is over.
    let each = r.node(run.id, "each");
    assert_eq!((each.status, each.note.clone(), each.looping.as_ref().map(|l| l.index)), (NodeStatus::Skipped, "the run failed before it".to_string(), Some(1)));
    // Trying the failed step again goes on at item 2, and does not do item 1 again.
    r.runner.retry_node("local", run.id, "get").unwrap();
    r.settle();
    assert_eq!(r.run(run.id).status, RunStatus::Succeeded, "{:?}", r.messages(run.id));
    assert_eq!(urls(&r, "get"), ["https://example.com/ann?i=0", "https://example.com/bob?i=1", "https://example.com/bob?i=1", "https://example.com/cy?i=2"]);
    assert_eq!(r.node(run.id, "each").outputs["count"], 3);
}

#[test]
fn a_step_inside_that_asks_the_person_waits_at_the_same_item_and_retry_skip_or_stop_all_work() {
    for choice in ["retry", "skip", "stop"] {
        let r = rig(&format!("loop_ask_{choice}"));
        r.fake.script("visit", vec![Step::Fatal("nope")]);
        let f = r.flow(json!({ "name": "Ask", "nodes": [trigger(), each("[\"a\", \"b\"]"), visit("ask_user"), notify("after", "{{nodes.each.count}} done")], "edges": [["start", "each"], ["each", "visit", "each"], ["each", "after", "done"]] }));
        let run = r.go_and_settle(f);
        assert_eq!(run.status, RunStatus::Waiting, "{choice}: {:?}", r.messages(run.id));
        assert_eq!((r.node(run.id, "visit").status, r.node(run.id, "after").status), (NodeStatus::NeedsAction, NodeStatus::Pending));
        let each = r.node(run.id, "each");
        assert_eq!((each.status, each.looping.as_ref().map(|l| (l.index, l.items.len()))), (NodeStatus::Running, Some((0, 2))), "the list waits at the first item");
        let inbox = r.runs.inbox("local").unwrap();
        assert!(matches!(&inbox[0].wait, WaitRequest::Failure { message, .. } if message.contains("\"visit\" failed on item 1 of 2: nope")), "{inbox:?}");
        r.respond(run.id, "visit", Response { choice: Some(choice.into()), ..Default::default() }).unwrap();
        let done = r.run(run.id);
        match choice {
            "retry" => {
                assert_eq!(done.status, RunStatus::Succeeded, "{:?}", r.messages(run.id));
                assert_eq!(urls(&r, "visit"), ["https://example.com/a", "https://example.com/a", "https://example.com/b"], "the same item again, then the next");
            }
            "skip" => {
                assert_eq!(done.status, RunStatus::Succeeded, "{:?}", r.messages(run.id));
                assert_eq!(urls(&r, "visit"), ["https://example.com/a", "https://example.com/b"], "the item was gone past, and the list went on");
            }
            _ => {
                assert_eq!(done.status, RunStatus::Cancelled);
                assert_eq!(urls(&r, "visit"), ["https://example.com/a"]);
                assert_eq!(r.node(run.id, "each").status, NodeStatus::Skipped, "a stopped run does not leave the loop running");
            }
        }
    }
}

// ---- stop, pause, a crash -----------------------------------------------------------------------------------------------------

#[test]
fn a_stop_in_the_middle_of_the_list_ends_the_run_and_does_not_start_the_next_item() {
    let r = rig("loop_stop");
    reads(&r, names(&["ann", "bob", "cy"]));
    let f = get_flow(&r, "fail");
    let _gate = r.fake.gate("get");
    let run = r.start(f);
    until("the first item to be running", || r.fake.count("get") == 1);
    r.runner.stop("local", run.id).unwrap();
    r.settle();
    let stopped = r.run(run.id);
    assert_eq!(stopped.status, RunStatus::Cancelled);
    assert_eq!(r.fake.count("get"), 1, "the next item was not started");
    let each = r.node(run.id, "each");
    assert_eq!((each.status, each.note.clone()), (NodeStatus::Skipped, "the run was stopped".to_string()));
    assert_eq!(r.node(run.id, "after").status, NodeStatus::Skipped);
    assert!(r.runner.stop("local", run.id).unwrap_err().contains("finished"));
}

#[test]
fn a_paused_for_each_goes_on_at_the_item_it_was_on_and_every_item_has_its_own_ledger_line() {
    let r = rig("loop_pause");
    reads(&r, names(&["ann", "bob", "cy"]));
    let f = send_flow(&r);
    let gate = r.fake.gate("send");
    let run = r.start(f);
    until("the first send to be running", || r.fake.count("send") == 1);
    r.runner.pause("local", run.id).unwrap();
    gate.store(true, Ordering::SeqCst);
    r.settle();
    assert_eq!(r.run(run.id).status, RunStatus::Paused);
    // It stopped between two items: the first is done, the list is still at it, and nothing else was started.
    let each = r.node(run.id, "each");
    let place = each.looping.clone().expect("the step keeps its place");
    assert_eq!((each.status, place.index, place.fresh, place.results.len(), r.fake.count("send")), (NodeStatus::Running, 0, false, 0, 1));
    assert_eq!(r.runs.effect_get(&format!("{}:send:0", run.id)).unwrap().unwrap().0, "done");
    assert!(r.runs.effect_get(&format!("{}:send", run.id)).unwrap().is_none(), "the line of a step inside a For each step has the item in it");
    r.runner.resume("local", run.id).unwrap();
    r.settle();
    assert_eq!(r.run(run.id).status, RunStatus::Succeeded, "{:?}", r.messages(run.id));
    let texts: Vec<String> = r.fake.calls_to("send").iter().map(|c| c["text"].as_str().unwrap().to_string()).collect();
    assert_eq!(texts, ["ann", "bob", "cy"], "the first item was not sent again");
    for i in 0..3 {
        assert_eq!(r.runs.effect_get(&format!("{}:send:{i}", run.id)).unwrap().unwrap().0, "done", "item {i}");
    }
}

#[test]
fn after_a_restart_a_new_runner_on_the_same_database_goes_on_at_the_item_and_does_not_send_the_first_again() {
    let r = rig("loop_restart");
    reads(&r, names(&["ann", "bob", "cy"]));
    let f = send_flow(&r);
    let gate = r.fake.gate("send");
    let run = r.start(f);
    until("the first send to be running", || r.fake.count("send") == 1);
    r.runner.pause("local", run.id).unwrap();
    gate.store(true, Ordering::SeqCst);
    r.settle();
    assert_eq!((r.run(run.id).status, r.fake.count("send")), (RunStatus::Paused, 1));
    // Aqua is killed here: the run is marked running, as it would be left, and the next start finds it.
    r.runs.set_status(run.id, RunStatus::Running, "", 500).unwrap();
    assert_eq!(r.runner.recover(), 1);
    let each = r.node(run.id, "each");
    assert_eq!((r.run(run.id).status, each.status, each.looping.as_ref().map(|l| l.index)), (RunStatus::Interrupted, NodeStatus::Interrupted, Some(0)));
    let (second, fake2) = second_runner(&r);
    second.resume("local", run.id).unwrap();
    assert!(second.wait_idle(Duration::from_secs(15)), "the run did not settle");
    assert_eq!(r.run(run.id).status, RunStatus::Succeeded, "{:?}", r.messages(run.id));
    assert_eq!(r.fake.count("send"), 1, "the first runner sent one");
    let later: Vec<String> = fake2.calls_to("send").iter().map(|c| c["text"].as_str().unwrap().to_string()).collect();
    assert_eq!(later, ["bob", "cy"], "the new runner did the rest and not the first again");
    let each = r.node(run.id, "each");
    assert_eq!((each.outputs["count"].clone(), each.outputs["results"].as_array().map(|a| a.len())), (json!(3), Some(3)));
    assert!(r.messages(run.id).iter().any(|m| m.contains("going on at item 1 of 3")), "{:?}", r.messages(run.id));
}

/// The state of a run that was killed while the item at `at` was being sent, made by hand as `after_a_crash_...` does: the ledger knows about the items before it.
fn killed_while_sending(r: &Rig, f: i64, at: usize) -> Run {
    let flow = r.flows.get("local", f).unwrap().unwrap();
    let run = r.runs.create_run("local", &flow, Map::new(), Value::Null, "manual", false, None, 100).unwrap();
    let rows = names(&["ann", "bob", "cy"]);
    let set = |id: &str, change: &dyn Fn(&mut NodeRun)| {
        let mut nr = r.runs.node_run(run.id, id).unwrap().unwrap();
        change(&mut nr);
        r.runs.save_node(&nr).unwrap();
    };
    set("start", &|nr| {
        nr.status = NodeStatus::Succeeded;
        nr.taken = vec!["out".into()];
    });
    set("read", &|nr| {
        nr.status = NodeStatus::Succeeded;
        nr.taken = vec!["out".into()];
        nr.outputs = [("rows".to_string(), rows.clone())].into_iter().collect();
    });
    let items = rows.as_array().unwrap().clone();
    set("each", &|nr| {
        nr.status = NodeStatus::Running;
        nr.outputs = crate::flows::looping::loop_outputs(items[at].clone(), at, 3);
        nr.looping = Some(LoopState { items: items.clone(), index: at, fresh: false, results: (0..at).map(|_| json!({ "send": { "status": "published" } })).collect() });
    });
    set("send", &|nr| nr.status = NodeStatus::Running);
    for i in 0..at {
        let key = format!("{}:send:{i}", run.id);
        assert!(r.runs.effect_begin(&key, run.id, "send", 101).unwrap());
        r.runs.effect_done(&key, &json!({ "status": "published", "post_url": format!("https://example.com/p{i}") }), 102).unwrap();
    }
    assert!(r.runs.effect_begin(&format!("{}:send:{at}", run.id), run.id, "send", 103).unwrap());
    r.runs.set_status(run.id, RunStatus::Running, "", 104).unwrap();
    assert_eq!(r.runner.recover(), 1);
    run
}

#[test]
fn an_item_that_was_being_sent_when_aqua_stopped_is_asked_about_and_the_items_before_it_are_not_sent_again() {
    let r = rig("loop_ledger_started");
    let f = send_flow(&r);
    let run = killed_while_sending(&r, f, 1);
    r.runner.resume("local", run.id).unwrap();
    r.settle();
    assert_eq!(r.run(run.id).status, RunStatus::Waiting, "it does not guess");
    assert_eq!(r.node(run.id, "send").status, NodeStatus::NeedsAction);
    assert_eq!(r.fake.count("send"), 0, "nothing was sent again: not the first item, not the one that may have gone");
    let each = r.node(run.id, "each");
    assert_eq!((each.status, each.looping.as_ref().map(|l| l.index)), (NodeStatus::Running, Some(1)), "the list waits at the item");
    // "It was done": recorded for that item, and the list goes on with the last one.
    r.respond(run.id, "send", Response { choice: Some("done".into()), answer: Some("https://example.com/post/b".into()), ..Default::default() }).unwrap();
    assert_eq!(r.run(run.id).status, RunStatus::Succeeded, "{:?}", r.messages(run.id));
    let sent: Vec<String> = r.fake.calls_to("send").iter().map(|c| c["text"].as_str().unwrap().to_string()).collect();
    assert_eq!(sent, ["cy"], "only the item after it was sent");
    for i in 0..3 {
        assert_eq!(r.runs.effect_get(&format!("{}:send:{i}", run.id)).unwrap().unwrap().0, "done", "item {i}");
    }
    let results = r.node(run.id, "each").outputs["results"].as_array().unwrap().clone();
    assert_eq!((results.len(), results[1]["send"]["post_url"].clone()), (3, json!("https://example.com/post/b")));
}

#[test]
fn an_item_that_was_sent_but_not_saved_as_finished_is_taken_as_done_and_not_sent_again() {
    let r = rig("loop_ledger_done");
    let f = send_flow(&r);
    let run = killed_while_sending(&r, f, 1);
    // The item was sent and written down, then Aqua stopped before the step could say so.
    r.runs.effect_done(&format!("{}:send:1", run.id), &json!({ "status": "published", "post_url": "https://example.com/p1" }), 105).unwrap();
    r.runner.resume("local", run.id).unwrap();
    r.settle();
    assert_eq!(r.run(run.id).status, RunStatus::Succeeded, "{:?}", r.messages(run.id));
    let sent: Vec<String> = r.fake.calls_to("send").iter().map(|c| c["text"].as_str().unwrap().to_string()).collect();
    assert_eq!(sent, ["cy"], "ann and bob were not sent again");
    let results = r.node(run.id, "each").outputs["results"].as_array().unwrap().clone();
    assert_eq!(results[1]["send"]["post_url"], "https://example.com/p1");
}

// ---- dry run -----------------------------------------------------------------------------------------------------------------

#[test]
fn a_dry_run_goes_through_every_item_with_the_dry_run_rules_and_a_simulated_list_runs_once_with_a_sample() {
    let r = rig("loop_dry");
    let publish_each = n("send", "social_media", json!({ "platform": "linkedin", "action": "publish", "text": "{{nodes.each.item}}" }));
    let f = r.flow(json!({ "name": "Dry each", "nodes": [trigger(), each("[\"a\", \"b\", \"c\"]"), publish_each.clone(), notify("after", "{{nodes.each.count}} items")],
        "edges": [["start", "each"], ["each", "send", "each"], ["each", "after", "done"]] }));
    let run = r.runner.start("local", f, Map::new(), Value::Null, true, "manual").unwrap();
    r.settle();
    let done = r.run(run.id);
    assert_eq!((done.status, done.dry_run), (RunStatus::Succeeded, true), "{:?}", r.messages(run.id));
    assert_eq!(r.fake.total(), 0, "nothing outside was called");
    assert_eq!(r.messages(run.id).iter().filter(|m| m.as_str() == "Would run \"send\" here. Nothing was sent.").count(), 3, "once for every item");
    assert!(r.messages(run.id).iter().any(|m| m == "3 items"));
    assert!(r.runs.effect_get(&format!("{}:send:0", run.id)).unwrap().is_none(), "no ledger line: nothing happened");

    // A list from a step that was only simulated is a line of text that says so: the steps inside run once, with it.
    let r = rig("loop_dry_sample");
    let f = r.flow(json!({ "name": "Dry sample", "nodes": [trigger(), read_step(), each("{{nodes.read.rows}}"), publish_each, notify("after", "{{nodes.each.count}} items")],
        "edges": [["start", "read"], ["read", "each"], ["each", "send", "each"], ["each", "after", "done"]] }));
    let run = r.runner.start("local", f, Map::new(), Value::Null, true, "manual").unwrap();
    r.settle();
    assert_eq!(r.run(run.id).status, RunStatus::Succeeded, "{:?}", r.messages(run.id));
    assert_eq!(r.fake.total(), 0);
    assert_eq!(r.events(run.id).iter().filter(|e| e.kind == "loop_item").count(), 1);
    assert!(r.messages(run.id).iter().any(|m| m.contains("a sample: the list comes from a step that was only simulated")), "{:?}", r.messages(run.id));
}

// ---- people: approvals --------------------------------------------------------------------------------------------------------

#[test]
fn an_approval_inside_a_for_each_asks_for_every_item_and_a_rejection_skips_what_it_guards_for_that_item_only() {
    let r = rig("loop_approval");
    reads(&r, names(&["ann", "bob"]));
    let f = per_row(&r, "all done", vec![n("ok", "approval", json!({ "title": "Go?", "preview": "{{nodes.each.item.name}}" })), visit_row()], vec![json!(["each", "ok", "each"]), json!(["ok", "visit", "approved"])]);
    let run = r.go_and_settle(f);
    assert_eq!(run.status, RunStatus::Waiting);
    let first = r.runs.inbox("local").unwrap();
    assert!(first.len() == 1 && matches!(&first[0].wait, WaitRequest::Approval { preview, implicit: false, .. } if preview == "ann"), "{first:?}");
    r.respond(run.id, "ok", approve()).unwrap();
    // The first item went through, and the second asks again.
    assert_eq!(urls(&r, "visit"), ["https://example.com/ann"]);
    let second = r.runs.inbox("local").unwrap();
    assert!(second.len() == 1 && matches!(&second[0].wait, WaitRequest::Approval { preview, .. } if preview == "bob"), "{second:?}");
    r.respond(run.id, "ok", Response { decision: Some("rejected".into()), ..Default::default() }).unwrap();
    assert_eq!(r.run(run.id).status, RunStatus::Succeeded, "{:?}", r.messages(run.id));
    assert_eq!(urls(&r, "visit"), ["https://example.com/ann"], "bob was not approved");
    let results = r.node(run.id, "each").outputs["results"].as_array().unwrap().clone();
    assert!(results[0].get("visit").is_some() && results[1].get("visit").is_none() && results[1]["ok"]["decision"] == "rejected", "{results:?}");
}

fn visit_row() -> Value {
    n("visit", "http", json!({ "method": "GET", "url": "https://example.com/{{nodes.each.item.name}}" }))
}

#[test]
fn a_step_with_an_outside_effect_inside_a_for_each_asks_for_every_item_unless_an_approval_comes_before_the_loop() {
    // No approval step: the step asks by itself, and asks again for the next item.
    let r = rig("loop_gate");
    reads(&r, names(&["ann", "bob"]));
    let flow = || json!({ "name": "Gate", "nodes": [trigger(), read_step(), each("{{nodes.read.rows}}"), n("send", "social_media", json!({ "platform": "linkedin", "action": "publish", "text": "{{nodes.each.item.name}}" })), notify("after", "{{nodes.each.count}} sent")],
        "edges": [["start", "read"], ["read", "each"], ["each", "send", "each"], ["each", "after", "done"]] });
    let said = super::super::validate::validate(&def(flow()));
    assert!(said.issues.iter().any(|i| i.code == "will_ask" && i.message.contains("for every item")), "{:#?}", said.issues);
    let f = r.flow(flow());
    let run = r.go_and_settle(f);
    assert_eq!(run.status, RunStatus::Waiting);
    assert!(matches!(&r.runs.inbox("local").unwrap()[0].wait, WaitRequest::Approval { implicit: true, .. }));
    r.respond(run.id, "send", approve()).unwrap();
    assert_eq!((r.run(run.id).status, r.fake.count("send")), (RunStatus::Waiting, 1), "the second item asks again");
    r.respond(run.id, "send", approve()).unwrap();
    assert_eq!((r.run(run.id).status, r.fake.count("send")), (RunStatus::Succeeded, 2));

    // An Approval step before the loop covers all of its items: it is asked once.
    let r = rig("loop_gate_before");
    reads(&r, names(&["ann", "bob"]));
    let f = r.flow(json!({ "name": "Gate before", "nodes": [trigger(), read_step(), n("ok", "approval", json!({ "title": "Post to everyone?", "preview": "Post to {{nodes.read.rows | length}} people?" })), each("{{nodes.read.rows}}"), n("send", "social_media", json!({ "platform": "linkedin", "action": "publish", "text": "{{nodes.each.item.name}}" })), notify("after", "{{nodes.each.count}} sent")],
        "edges": [["start", "read"], ["read", "ok"], ["ok", "each", "approved"], ["each", "send", "each"], ["each", "after", "done"]] }));
    let run = r.go_and_settle(f);
    assert_eq!(run.status, RunStatus::Waiting);
    r.respond(run.id, "ok", approve()).unwrap();
    assert_eq!((r.run(run.id).status, r.fake.count("send")), (RunStatus::Succeeded, 2), "one question, two posts");
}

// ---- what a loop does when it cannot run -------------------------------------------------------------------------------------------

#[test]
fn a_switched_off_for_each_is_skipped_with_what_is_inside_it_and_the_flow_goes_on_by_done() {
    let r = rig("loop_off");
    reads(&r, names(&["ann"]));
    let f = r.flow(json!({ "name": "Off", "nodes": [trigger(), read_step(), { "id": "each", "kind": "loop", "name": "each", "enabled": false, "config": { "items": "{{nodes.read.rows}}" } }, get("fail"), notify("after", "all done")],
        "edges": [["start", "read"], ["read", "each"], ["each", "get", "each"], ["each", "after", "done"]] }));
    let run = r.go_and_settle(f);
    assert_eq!(run.status, RunStatus::Succeeded, "{:?}", r.messages(run.id));
    assert_eq!(r.fake.count("get"), 0, "what is inside did not run once without an item");
    assert_eq!((r.node(run.id, "each").status, r.node(run.id, "get").status, r.node(run.id, "after").status), (NodeStatus::Skipped, NodeStatus::Skipped, NodeStatus::Succeeded));
}

#[test]
fn a_for_each_that_cannot_read_its_list_and_is_set_to_carry_on_goes_on_by_done_and_not_by_each() {
    let r = rig("loop_carry_on");
    reads(&r, json!({ "not": "a list" }));
    let mut lp = each("{{nodes.read.rows}}");
    lp["on_error"] = json!("continue");
    let f = r.flow(json!({ "name": "Carry on", "nodes": [trigger(), read_step(), lp, get("fail"), notify("after", "all done")],
        "edges": [["start", "read"], ["read", "each"], ["each", "get", "each"], ["each", "after", "done"]] }));
    let run = r.go_and_settle(f);
    assert_eq!(run.status, RunStatus::Succeeded, "{:?}", r.messages(run.id));
    assert_eq!((r.node(run.id, "each").status, r.node(run.id, "each").taken.clone()), (NodeStatus::Failed, vec!["done".to_string()]));
    assert_eq!((r.fake.count("get"), r.node(run.id, "after").status), (0, NodeStatus::Succeeded));
}

// ---- the checker ---------------------------------------------------------------------------------------------------------------

fn issues_of(v: Value) -> Vec<(String, String)> {
    let report = crate::flows::validate::validate(&def(v));
    report.issues.into_iter().map(|i| (i.code, i.message)).collect()
}

fn has(issues: &[(String, String)], code: &str) -> bool {
    issues.iter().any(|(c, _)| c == code)
}

#[test]
fn the_checker_reads_a_written_list_the_way_the_engine_does_and_refuses_what_cannot_be_read() {
    let flow = |items: Value| json!({ "name": "List", "nodes": [trigger(), n("each", "loop", json!({ "items": items })), notify("tell", "x")], "edges": [["start", "each"], ["each", "tell", "each"]] });
    for fine in [json!("a,b"), json!("one\ntwo"), json!("[\"a\", 2]"), json!("{{vars.list}} ")] {
        let mut f = flow(fine.clone());
        f["variables"] = json!([{ "name": "list", "label": "List", "kind": "text", "required": false, "default": "x" }]);
        let issues = issues_of(f);
        assert!(!issues.iter().any(|(c, _)| matches!(c.as_str(), "bad_list" | "too_many_items" | "missing_config" | "bad_config")), "{fine}: {issues:?}");
    }
    assert!(has(&issues_of(flow(json!("[\"a\", ]"))), "bad_list"), "a JSON list that is not one");
    assert!(has(&issues_of(flow(json!("{\"a\": 1}"))), "bad_list"), "a record");
    assert!(has(&issues_of(flow(json!(5))), "bad_list"), "a number");
    assert!(has(&issues_of(flow(json!(""))), "missing_config"), "nothing");
    let long = (0..201).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
    assert!(has(&issues_of(flow(json!(long))), "too_many_items"));
    let ok200 = (0..200).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
    assert!(!has(&issues_of(flow(json!(ok200))), "too_many_items"));
    // A loop with nothing on "each" is a warning, not an error.
    let none = issues_of(json!({ "name": "None", "nodes": [trigger(), each("a,b"), notify("tell", "x")], "edges": [["start", "each"], ["each", "tell", "done"]] }));
    assert!(none.iter().any(|(c, m)| c == "open_branch" && m.contains("\"each\"")) && !has(&none, "bad_list"), "{none:?}");
}

#[test]
fn a_for_each_inside_another_is_refused_and_so_is_a_connection_between_the_inside_and_the_outside() {
    // A loop inside a loop.
    let nested = issues_of(json!({ "name": "Nested", "nodes": [trigger(), each("a,b"), n("inner", "loop", json!({ "items": "c,d" })), notify("tell", "x")],
        "edges": [["start", "each"], ["each", "inner", "each"], ["inner", "tell", "each"]] }));
    assert!(nested.iter().any(|(c, m)| c == "nested_loop" && m.contains("not supported")), "{nested:?}");
    let report = crate::flows::validate::validate(&def(json!({ "name": "Nested", "nodes": [trigger(), each("a,b"), n("inner", "loop", json!({ "items": "c,d" })), notify("tell", "x")], "edges": [["start", "each"], ["each", "inner", "each"], ["inner", "tell", "each"]] })));
    assert!(!report.ok(), "a flow with a loop in a loop does not run");
    // A step reached from "each" that is also reached from "done".
    let leak = issues_of(json!({ "name": "Leak", "nodes": [trigger(), each("a,b"), notify("a", "x"), notify("tail", "y")], "edges": [["start", "each"], ["each", "a", "each"], ["a", "tail"], ["each", "tail", "done"]] }));
    assert!(leak.iter().any(|(c, m)| c == "loop_body_open" && m.contains("\"done\"")), "{leak:?}");
    // A step inside that is also fed from outside the loop.
    let outside = issues_of(json!({ "name": "Outside", "nodes": [trigger(), notify("before", "b"), each("a,b"), notify("a", "x")], "edges": [["start", "before"], ["before", "each"], ["before", "a"], ["each", "a", "each"]] }));
    assert!(outside.iter().any(|(c, m)| c == "loop_body_open" && m.contains("outside it")), "{outside:?}");
    // A step after the loop that reads a step inside it is pointed at the loop's results.
    let reads_inside = issues_of(json!({ "name": "Reads", "nodes": [trigger(), each("a,b"), text("write", "w"), notify("after", "{{nodes.write.text}}")], "edges": [["start", "each"], ["each", "write", "each"], ["each", "after", "done"]] }));
    assert!(reads_inside.iter().any(|(c, m)| c == "not_upstream" && m.contains("inside the For each step") && m.contains("{{nodes.each.results}}")), "{reads_inside:?}");
    // A well-made loop has none of these.
    let fine = issues_of(json!({ "name": "Fine", "nodes": [trigger(), each("a,b"), text("write", "w {{nodes.each.item}}"), notify("after", "{{nodes.each.results[*].write.text}}")], "edges": [["start", "each"], ["each", "write", "each"], ["each", "after", "done"]] }));
    assert!(!fine.iter().any(|(c, _)| matches!(c.as_str(), "nested_loop" | "loop_body_open" | "not_upstream" | "missing_output" | "bad_list")), "{fine:?}");
}

/// Found while building the For each step: the checker read `rows[0]` or `results[*]` as the name of an output and refused every reference that looked inside one.
#[test]
fn a_reference_that_looks_into_an_output_is_checked_by_the_name_before_the_bracket() {
    let flow = |message: &str| json!({ "name": "Look inside", "nodes": [trigger(), read_step(), notify("tell", message)], "edges": [["start", "read"], ["read", "tell"]] });
    let fine = issues_of(flow("{{nodes.read.rows[0].name}} and {{nodes.read.rows[*].name | join: \", \"}}"));
    assert!(!has(&fine, "missing_output") && !has(&fine, "bad_template"), "{fine:?}");
    let bad = issues_of(flow("{{nodes.read.nope[0]}}"));
    assert!(has(&bad, "missing_output"), "a name that is not an output is still refused: {bad:?}");
}

#[test]
fn a_flow_with_a_loop_inside_a_loop_is_refused_before_a_run_starts() {
    let r = rig("loop_nested_refused");
    let d = def(json!({ "name": "Nested", "nodes": [trigger(), each("a,b"), n("inner", "loop", json!({ "items": "c,d" })), notify("tell", "x")], "edges": [["start", "each"], ["each", "inner", "each"], ["inner", "tell", "each"]] }));
    let f = r.flows.create("local", d, "user", "", 100).unwrap().id;
    let e = r.runner.start("local", f, Map::new(), Value::Null, false, "manual").unwrap_err();
    assert!(e.contains("has problems") && e.contains("not supported"), "{e}");
}

#[test]
fn going_on_from_a_step_inside_a_for_each_is_refused_and_pointed_at_the_loop() {
    let r = rig("loop_run_from");
    reads(&r, names(&["ann"]));
    let f = get_flow(&r, "fail");
    let first = r.go_and_settle(f);
    assert_eq!(first.status, RunStatus::Succeeded);
    let e = r.runner.run_from("local", first.id, "get").unwrap_err();
    assert!(e.contains("inside the For each step \"each\"") && e.contains("run from \"each\" instead"), "{e}");
    // From the loop itself it works, and does the whole list again.
    reads(&r, names(&["ann"]));
    let again = r.runner.run_from("local", first.id, "each").unwrap();
    r.settle();
    assert_eq!((r.run(again.id).status, r.fake.count("get")), (RunStatus::Succeeded, 2));
}

#[test]
fn a_database_made_before_the_for_each_step_gets_a_column_for_the_place_in_the_list_when_it_is_opened() {
    let dir = std::env::temp_dir().join(format!("aqua_runs_loop_old_db_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("flows.db");
    {
        // The table as it was before the column: one record of a step that was running.
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(
            "CREATE TABLE flow_node_runs (run_id INTEGER NOT NULL, node_id TEXT NOT NULL, status TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, started_at INTEGER NOT NULL DEFAULT 0,
             finished_at INTEGER NOT NULL DEFAULT 0, outputs_json TEXT NOT NULL DEFAULT '{}', taken_json TEXT NOT NULL DEFAULT '[]', error TEXT NOT NULL DEFAULT '', wait_json TEXT,
             gate_approved INTEGER NOT NULL DEFAULT 0, note TEXT NOT NULL DEFAULT '', PRIMARY KEY (run_id, node_id));
             INSERT INTO flow_node_runs (run_id, node_id, status) VALUES (1, 'each', 'running');",
        )
        .unwrap();
    }
    let store = RunStore::new(&path);
    let mut nr = store.node_run(1, "each").unwrap().expect("the old record is still there");
    assert_eq!((nr.status, nr.looping.is_none()), (NodeStatus::Running, true));
    nr.looping = Some(LoopState { items: vec![json!("a"), json!("b")], index: 1, fresh: false, results: vec![json!({ "x": { "y": 1 } })] });
    store.save_node(&nr).unwrap();
    assert_eq!(store.node_run(1, "each").unwrap().unwrap().looping, nr.looping, "the place is kept and read back");
    // Opening it again changes nothing.
    let again = RunStore::new(&path);
    assert_eq!(again.node_run(1, "each").unwrap().unwrap().looping, nr.looping);
    let _ = std::fs::remove_dir_all(&dir);
}

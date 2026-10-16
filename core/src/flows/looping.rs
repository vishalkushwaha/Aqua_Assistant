//! The For each step, the parts that need no run: how a list is read, the limits, which steps are inside the loop, and the key that keeps an outside action from being done
//! twice for the same item. The checker (`validate`) and the engine (`run::engine`) both use these, so a flow is judged the way it will be run (card R7-12).
//!
//! What the step does, in short: the steps connected to its "each" exit run once for every item of the list, one item after another (the steps inside one item
//! may run side by side, as anywhere); then the steps connected to "done" go on. While an item runs, `{{nodes.<loop>.item}}` is the item, `.index` counts from 0 and
//! `.number` from 1, `.count` is the length of the list. When the list is done, `{{nodes.<loop>.results}}` holds one entry for each item: the step ids inside, and what each produced.

use super::model::{FlowDef, NodeKind};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet, VecDeque};

/// The most items a For each step runs, and the most step runs: the number of items times the number of steps inside (every one that is switched on counts, whether or not a
/// condition lets it run). Over either limit the step stops before it begins, with a message; a list is never cut short without saying so.
pub const MAX_ITEMS: usize = 200;
pub const MAX_STEP_RUNS: usize = 2000;
/// The most the collected results of one For each step may take (as JSON), and the longest text kept in them. The steps inside keep their full outputs for the item they are on.
pub const MAX_RESULT_BYTES: usize = 2_000_000;
pub const MAX_RESULT_TEXT: usize = 20_000;

/// The ids of the steps inside the For each step `loop_id`: everything that can be reached from its "each" exit, in the order of the flow's step list.
pub fn body_of(def: &FlowDef, loop_id: &str) -> Vec<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<&str> = def.edges.iter().filter(|e| e.from == loop_id && e.port == "each").map(|e| e.to.as_str()).collect();
    while let Some(cur) = queue.pop_front() {
        if cur == loop_id || !seen.insert(cur) {
            continue;
        }
        queue.extend(def.edges.iter().filter(|e| e.from == cur).map(|e| e.to.as_str()));
    }
    def.nodes.iter().filter(|n| seen.contains(n.id.as_str())).map(|n| n.id.clone()).collect()
}

/// For every For each step of the flow: the ids of the steps inside it.
pub fn bodies(def: &FlowDef) -> HashMap<String, Vec<String>> {
    def.nodes.iter().filter(|n| n.kind == NodeKind::Loop).map(|n| (n.id.clone(), body_of(def, &n.id))).collect()
}

/// The For each step that `node_id` is inside, if any. (A step inside two is a flow the checker refuses; the first is returned.)
pub fn enclosing_loop<'a>(def: &'a FlowDef, node_id: &str) -> Option<&'a str> {
    def.nodes.iter().filter(|n| n.kind == NodeKind::Loop && n.id != node_id).find(|l| body_of(def, &l.id).iter().any(|b| b == node_id)).map(|l| l.id.as_str())
}

/// The item that the For each step around `node_id` is on, given a way to ask a For each step which item it is on. `None` for a step that is not inside one.
pub fn item_of(def: &FlowDef, node_id: &str, index_of_loop: impl Fn(&str) -> Option<usize>) -> Option<usize> {
    enclosing_loop(def, node_id).and_then(index_of_loop)
}

/// The key under which the ledger remembers that an outside action was started or done. A step inside a For each step has one for each item, so doing item 3 never looks
/// like doing item 1 again, and a crash in the middle of item 3 asks about item 3 only.
pub fn effect_key(run_id: i64, node_id: &str, item: Option<usize>) -> String {
    match item {
        Some(i) => format!("{run_id}:{node_id}:{i}"),
        None => format!("{run_id}:{node_id}"),
    }
}

/// A list as a For each step reads it. A list stays a list; nothing is no items; text is read by [`list_from_text`]; a record, a number or a yes/no is not a list.
pub fn read_list(v: &Value) -> Result<Vec<Value>, String> {
    match v {
        Value::Array(a) => Ok(a.clone()),
        Value::Null => Ok(vec![]),
        Value::String(s) => list_from_text(s),
        Value::Object(_) => Err("the list is a single record (an object), not a list. Point it at a list, for example the rows of a spreadsheet step".into()),
        Value::Bool(_) | Value::Number(_) => Err(format!("the list is {v}, which is not a list")),
    }
}

/// Text as a list. A JSON list (starts with `[` and ends with `]`) is read as one and must be valid. Anything else is one item for each line; a text of a single line is
/// cut at its commas. Items are trimmed and empty ones dropped.
pub fn list_from_text(text: &str) -> Result<Vec<Value>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(vec![]);
    }
    if t.starts_with('[') && t.ends_with(']') {
        return match serde_json::from_str::<Value>(t) {
            Ok(Value::Array(a)) => Ok(a),
            Ok(_) => Err("the list is not a JSON list".into()),
            Err(e) => Err(format!("the list starts with [ and ends with ], so it is read as a JSON list, but it is not one ({e}). Write it like [\"a\", \"b\"], or put one item on each line")),
        };
    }
    if t.starts_with('{') && t.ends_with('}') {
        return Err("the list is a single record (an object), not a list. Write a JSON list like [{...}, {...}], or put one item on each line".into());
    }
    let lines: Vec<&str> = t.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let parts: Vec<&str> = if lines.len() == 1 { lines[0].split(',').map(str::trim).filter(|p| !p.is_empty()).collect() } else { lines };
    Ok(parts.into_iter().map(|p| Value::String(p.to_string())).collect())
}

/// May a For each step with `items` items and `steps_inside` steps (switched on) inside it begin? The two limits, in plain words.
pub fn check_limits(items: usize, steps_inside: usize) -> Result<(), String> {
    if items > MAX_ITEMS {
        return Err(format!(
            "the list has {items} items, and a For each step runs at most {MAX_ITEMS}. Nothing was run. Make the list shorter (for example lower \"Most rows to read\" on a spreadsheet step, or put a LIMIT on the query) or split the work between two flows"
        ));
    }
    let runs = items.saturating_mul(steps_inside);
    if runs > MAX_STEP_RUNS {
        return Err(format!(
            "{items} items with {steps_inside} steps inside could run {runs} steps, and a For each step runs at most {MAX_STEP_RUNS}. Nothing was run. Use a shorter list or fewer steps inside"
        ));
    }
    Ok(())
}

/// What a For each step shows while it is on an item (and, for the last item, after it is done).
pub fn loop_outputs(item: Value, index: usize, count: usize) -> Map<String, Value> {
    let mut o = Map::new();
    o.insert("item".into(), item);
    o.insert("index".into(), json!(index));
    o.insert("number".into(), json!(index + 1));
    o.insert("count".into(), json!(count));
    o
}

/// A value for the collected results: texts longer than [`MAX_RESULT_TEXT`] are cut, and the cut says how long the text was.
pub fn cut_long_text(v: &Value) -> Value {
    match v {
        Value::String(s) if s.chars().count() > MAX_RESULT_TEXT => Value::String(format!("{}… [cut, {} characters]", s.chars().take(MAX_RESULT_TEXT).collect::<String>(), s.chars().count())),
        Value::Array(a) => Value::Array(a.iter().map(cut_long_text).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), cut_long_text(x))).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flows::model::{Edge, Node};

    fn flow(nodes: &[(&str, NodeKind)], edges: &[(&str, &str, &str)]) -> FlowDef {
        FlowDef { name: "t".into(), nodes: nodes.iter().map(|(id, k)| Node::new(id, *k, id)).collect(), edges: edges.iter().map(|(f, t, p)| Edge::new(f, t, p)).collect(), ..Default::default() }
    }

    #[test]
    fn a_list_is_read_as_a_json_list_as_lines_or_as_a_line_cut_at_its_commas() {
        assert_eq!(list_from_text("[\"a\", 2, {\"k\": 1}]").unwrap(), vec![json!("a"), json!(2), json!({"k": 1})]);
        assert_eq!(list_from_text("one\n two \n\n three").unwrap(), vec![json!("one"), json!("two"), json!("three")]);
        assert_eq!(list_from_text("a, b ,,c").unwrap(), vec![json!("a"), json!("b"), json!("c")], "one line is cut at its commas");
        assert_eq!(list_from_text("a, b\nc").unwrap(), vec![json!("a, b"), json!("c")], "several lines are not cut at commas");
        assert!(list_from_text("  \n ").unwrap().is_empty());
        // A malformed JSON list is an error and not a list of one odd line; text that only starts with a bracket is lines.
        assert!(list_from_text("[\"a\", ]").unwrap_err().contains("not one"));
        assert_eq!(list_from_text("[1] First source\n[2] Second source").unwrap(), vec![json!("[1] First source"), json!("[2] Second source")]);
        assert!(list_from_text("{\"a\": 1}").unwrap_err().contains("single record"));
        assert_eq!(list_from_text("{not json").unwrap(), vec![json!("{not json")]);
    }

    #[test]
    fn a_value_is_a_list_when_it_is_one_or_nothing_or_text_and_nothing_else_is() {
        assert_eq!(read_list(&json!([1, 2])).unwrap(), vec![json!(1), json!(2)]);
        assert!(read_list(&Value::Null).unwrap().is_empty());
        assert_eq!(read_list(&json!("x\ny")).unwrap().len(), 2);
        for not in [json!({"a": 1}), json!(5), json!(true)] {
            assert!(read_list(&not).is_err(), "{not}");
        }
    }

    #[test]
    fn the_limits_are_checked_before_anything_runs_and_say_what_to_do() {
        assert!(check_limits(200, 10).is_ok() && check_limits(0, 50).is_ok() && check_limits(1, 2000).is_ok());
        let items = check_limits(201, 1).unwrap_err();
        assert!(items.contains("201 items") && items.contains("at most 200") && items.contains("Nothing was run"), "{items}");
        let runs = check_limits(100, 21).unwrap_err();
        assert!(runs.contains("2100") && runs.contains("at most 2000") && runs.contains("Nothing was run"), "{runs}");
        assert!(check_limits(usize::MAX, usize::MAX).is_err(), "no overflow");
    }

    #[test]
    fn the_steps_inside_are_those_reached_from_the_each_exit_and_never_those_after_done() {
        let d = flow(
            &[("start", NodeKind::Trigger), ("each", NodeKind::Loop), ("a", NodeKind::Notification), ("b", NodeKind::Notification), ("after", NodeKind::Notification), ("tail", NodeKind::Notification)],
            &[("start", "each", "out"), ("each", "a", "each"), ("a", "b", "out"), ("each", "after", "done"), ("after", "tail", "out")],
        );
        assert_eq!(body_of(&d, "each"), vec!["a", "b"]);
        assert_eq!(enclosing_loop(&d, "b"), Some("each"));
        assert_eq!((enclosing_loop(&d, "after"), enclosing_loop(&d, "start"), enclosing_loop(&d, "each")), (None, None, None));
        assert_eq!(bodies(&d).len(), 1);
        assert_eq!(item_of(&d, "b", |l| (l == "each").then_some(4)), Some(4));
        assert_eq!(item_of(&d, "after", |_| Some(4)), None, "a step outside a loop has no item");
        // A loop with nothing on "each" has an empty body.
        let e = flow(&[("start", NodeKind::Trigger), ("each", NodeKind::Loop)], &[("start", "each", "out")]);
        assert!(body_of(&e, "each").is_empty());
    }

    #[test]
    fn an_item_has_its_own_ledger_key_and_a_step_outside_a_loop_keeps_the_old_one() {
        assert_eq!(effect_key(7, "send", None), "7:send");
        assert_eq!(effect_key(7, "send", Some(0)), "7:send:0");
        assert_ne!(effect_key(7, "send", Some(0)), effect_key(7, "send", Some(1)));
    }

    #[test]
    fn long_text_in_the_results_is_cut_and_says_how_long_it_was() {
        let long = "x".repeat(MAX_RESULT_TEXT + 5);
        let cut = cut_long_text(&json!({ "a": [long, "short"], "n": 3 }));
        let text = cut["a"][0].as_str().unwrap();
        assert!(text.ends_with(&format!("[cut, {} characters]", MAX_RESULT_TEXT + 5)) && text.chars().count() < MAX_RESULT_TEXT + 40, "{}", text.len());
        assert_eq!((cut["a"][1].clone(), cut["n"].clone()), (json!("short"), json!(3)));
    }

    #[test]
    fn what_a_loop_shows_while_it_is_on_an_item_counts_from_zero_and_from_one() {
        let o = loop_outputs(json!("b"), 1, 3);
        assert_eq!((o["item"].clone(), o["index"].clone(), o["number"].clone(), o["count"].clone()), (json!("b"), json!(1), json!(2), json!(3)));
    }
}

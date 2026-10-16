//! The For each step in the engine (card R7-12).
//!
//! How it runs. The step is *on an item* while its record says "running" and holds a place in its list (`NodeRun::looping`). Nothing runs it: the steps connected to its "each"
//! exit are ready as soon as the item begins, and go by the usual rules (a condition inside takes one side, independent steps run side by side, an approval or a question waits for
//! a person). When every step inside is finished, the item is over: what the steps made is added to `results`, the steps inside are put back to "not run yet", and the next item
//! begins. After the last item the step finishes by its "done" exit, and what is connected there goes on.
//!
//! Its place is written to the database before the steps inside are touched and after they are done, so a run that waits for a person, is paused, stopped or killed goes on at the
//! item it was on. Anything with an outside effect has its own ledger line for each item (`run:step:item`), so an item that was done is not done again.

use super::*;

impl State {
    /// A For each step that is on an item: it has no worker, and its "each" exit is open to the steps inside.
    pub(super) fn iterating(&self, id: &str) -> bool {
        self.nodes.get(id).is_some_and(|n| n.status == NodeStatus::Running && n.looping.is_some())
    }

    /// The For each step that `node_id` is inside, if any.
    fn loop_around(&self, node_id: &str) -> Option<&str> {
        self.bodies.iter().find(|(_, body)| body.iter().any(|b| b == node_id)).map(|(l, _)| l.as_str())
    }

    /// The item that the For each step around `node_id` is on (counted from 0). `None` for a step that is not inside one.
    pub(super) fn item_of(&self, node_id: &str) -> Option<usize> {
        let l = self.loop_around(node_id)?;
        self.nodes.get(l)?.looping.as_ref().map(|s| s.index)
    }

    /// The ledger key of a step: one for each item for a step inside a For each step.
    pub(super) fn effect_key(&self, node_id: &str) -> String {
        looping::effect_key(self.run.id, node_id, self.item_of(node_id))
    }

    /// "item 2 of 3" for a step inside a For each step, for the messages about it.
    pub(super) fn position(&self, node_id: &str) -> Option<String> {
        let l = self.loop_around(node_id)?;
        let s = self.nodes.get(l)?.looping.as_ref()?;
        Some(format!("item {} of {}", s.index + 1, s.items.len()))
    }

    /// Has this For each step something to do itself: begin its next item, or end it because the steps inside are all done?
    fn loop_ready(&self, id: &str) -> bool {
        let Some(s) = self.nodes.get(id).and_then(|n| n.looping.as_ref()) else { return false };
        self.iterating(id) && (s.fresh || self.bodies.get(id).is_some_and(|b| b.iter().all(|x| self.settled(x))))
    }

    /// Does any For each step have something to do itself? (A paused run is not over while one has.)
    pub(super) fn loop_has_work(&self) -> bool {
        self.bodies.keys().any(|id| self.loop_ready(id))
    }

    /// The name of a For each step that is on an item, for a run that found nothing more to do (it should not happen in a flow that was checked).
    pub(super) fn stuck_loop(&self, def: &FlowDef) -> Option<String> {
        let mut ids: Vec<&String> = self.bodies.keys().filter(|id| self.iterating(id)).collect();
        ids.sort();
        ids.first().map(|id| def.node(id).map(|n| n.name.clone()).unwrap_or_else(|| id.to_string()))
    }
}

impl Engine {
    /// For each For each step that is on an item: begin the item if it has not begun, end it if the steps inside are all done. Returns whether anything changed.
    pub(super) fn advance_loops(&self, st: &mut State, def: &FlowDef, failure: &mut Option<String>) -> bool {
        let mut changed = false;
        let mut ids: Vec<String> = st.bodies.keys().filter(|id| st.iterating(id)).cloned().collect();
        ids.sort();
        for id in ids {
            // An item with nothing inside it is over as soon as it begins, so one pass may go through several.
            while failure.is_none() && st.iterating(&id) {
                if st.nodes.get(&id).and_then(|n| n.looping.as_ref()).is_some_and(|s| s.fresh) {
                    self.begin_item(st, &id);
                } else if st.bodies.get(&id).is_some_and(|b| b.iter().all(|x| st.settled(x))) {
                    self.end_item(st, def, &id, failure);
                } else {
                    break;
                }
                changed = true;
            }
        }
        changed
    }

    /// Begin the item the step's place says: put every step inside back to "not run yet" (nothing of the item before is left, not even an approval), then show the item.
    fn begin_item(&self, st: &mut State, id: &str) {
        let run_id = st.run.id;
        for b in st.bodies.get(id).cloned().unwrap_or_default() {
            self.save(st, NodeRun::pending(run_id, &b));
        }
        let Some(mut nr) = st.nodes.get(id).cloned() else { return };
        let Some(state) = nr.looping.as_mut() else { return };
        // Only after the steps inside are reset: a crash in between is redone from "fresh".
        state.fresh = false;
        let (index, count) = (state.index, state.items.len());
        let item = state.items.get(index).cloned().unwrap_or(Value::Null);
        nr.outputs = looping::loop_outputs(item, index, count);
        nr.note = format!("item {} of {count}", index + 1);
        self.save(st, nr);
        let name = st.run.def.node(id).map(|n| n.name.clone()).unwrap_or_default();
        self.ev(run_id, Some(id), Level::Info, "loop_item", &format!("\"{name}\": item {} of {count}", index + 1));
    }

    /// The steps inside are all done: keep what they made, and go on to the next item or finish.
    fn end_item(&self, st: &mut State, def: &FlowDef, id: &str, failure: &mut Option<String>) {
        let (Some(node), Some(mut nr)) = (def.node(id), st.nodes.get(id).cloned()) else { return };
        let Some(mut state) = nr.looping.take() else { return };
        let mut made = Map::new();
        for b in st.bodies.get(id).map(Vec::as_slice).unwrap_or(&[]) {
            // A step that was skipped (a condition's other side, switched off) made nothing; one that failed and let the flow carry on shows that it did.
            if let Some(r) = st.nodes.get(b).filter(|r| matches!(r.status, NodeStatus::Succeeded | NodeStatus::Failed)) {
                made.insert(b.clone(), looping::cut_long_text(&Value::Object(r.outputs.clone())));
            }
        }
        state.results.push(Value::Object(made));
        state.index += 1;
        let count = state.items.len();
        let bytes = serde_json::to_string(&state.results).map(|s| s.len()).unwrap_or(usize::MAX);
        if bytes > looping::MAX_RESULT_BYTES {
            let why = format!(
                "what the steps inside made has grown past {} MB (after item {} of {count}), so nothing more was run. Use a shorter list, or have the steps inside write what they make to a file",
                looping::MAX_RESULT_BYTES / 1_000_000,
                state.index
            );
            self.fail_node(st, node, &why, 1, failure, false);
            return;
        }
        if state.index >= count {
            let note = format!("ran {count} item{}", if count == 1 { "" } else { "s" });
            self.settle_loop(st, node, &state.items, state.results.clone(), &note);
        } else {
            state.fresh = true;
            nr.looping = Some(state);
            self.save(st, nr);
        }
    }

    /// The step is done: it leaves by "done", with the last item and every item's results.
    fn settle_loop(&self, st: &mut State, node: &Node, items: &[Value], results: Vec<Value>, note: &str) {
        let count = items.len();
        let mut outputs = match items.last() {
            Some(last) => looping::loop_outputs(last.clone(), count - 1, count),
            None => {
                let mut o = looping::loop_outputs(Value::Null, 0, 0);
                o.insert("index".into(), Value::Null);
                o.insert("number".into(), Value::Null);
                o
            }
        };
        outputs.insert("results".into(), Value::Array(results));
        self.succeed(st, node, outputs, vec!["done".to_string()], note);
    }

    /// A For each step begins: read the list, check the limits, and put the step on its first item. Nothing is run if the list cannot be read or is over a limit.
    pub(super) fn start_loop(&self, st: &mut State, def: &FlowDef, node: &Node, cfg: &Value, failure: &mut Option<String>) {
        let run_id = st.run.id;
        let listed = cfg.get("items").cloned().unwrap_or(Value::Null);
        // In a dry run a list that comes from a step that was only simulated is a line of text that says so: the steps inside run once, with that line as the item.
        let simulated = st.run.dry_run && listed.as_str().is_some_and(|s| s.trim_start().starts_with("[dry run]"));
        let items = if simulated {
            vec![listed.clone()]
        } else {
            match looping::read_list(&listed) {
                Ok(v) => v,
                Err(e) => {
                    self.fail_node(st, node, &e, 1, failure, false);
                    return;
                }
            }
        };
        let inside = st.bodies.get(&node.id).map(|b| b.iter().filter(|x| def.node(x).is_some_and(|n| n.enabled)).count()).unwrap_or(0);
        if let Err(e) = looping::check_limits(items.len(), inside) {
            self.fail_node(st, node, &e, 1, failure, false);
            return;
        }
        if items.is_empty() {
            self.settle_loop(st, node, &[], vec![], "the list is empty, so the steps inside did not run");
            return;
        }
        let count = items.len();
        self.finish_node(st, &node.id, |n| n.looping = Some(LoopState { items, index: 0, fresh: true, results: vec![] }));
        let sample = if simulated { " (a sample: the list comes from a step that was only simulated)" } else { "" };
        self.ev(run_id, Some(&node.id), Level::Info, "loop_started", &format!("\"{}\": {count} item{}{sample}", node.name, if count == 1 { "" } else { "s" }));
    }

    /// A For each step that was left partway goes on at its item. Its list is the one it began with, not read again.
    pub(super) fn resume_loop(&self, st: &mut State, node: &Node) {
        let run_id = st.run.id;
        let Some(state) = st.nodes.get(&node.id).and_then(|n| n.looping.clone()) else { return };
        let (index, count) = (state.index, state.items.len());
        let item = state.items.get(index).cloned().unwrap_or(Value::Null);
        self.finish_node(st, &node.id, |n| {
            n.status = NodeStatus::Running;
            if n.started_at == 0 {
                n.started_at = now();
            }
            n.finished_at = 0;
            n.error.clear();
            n.wait = None;
            n.taken = vec![];
            n.outputs = looping::loop_outputs(item, index, count);
            n.note = format!("item {} of {count}", index + 1);
        });
        self.ev(run_id, Some(&node.id), Level::Info, "loop_resumed", &format!("\"{}\": going on at item {} of {count}", node.name, index + 1));
    }

    /// The run is ending (failed, or stopped) with a For each step on an item: the step is skipped, and keeps its place. If a step inside is tried again, or the run is carried on,
    /// the step goes on at that item. `why` is one of the notes that `FlowRunner` reopens a skipped step for.
    pub(super) fn park_loops(&self, st: &mut State, why: &str) {
        let ids: Vec<String> = st.bodies.keys().filter(|id| st.iterating(id)).cloned().collect();
        for id in ids {
            self.finish_node(st, &id, |n| {
                n.status = NodeStatus::Skipped;
                n.taken = vec![];
                n.finished_at = now();
                n.note = why.to_string();
            });
        }
    }
}

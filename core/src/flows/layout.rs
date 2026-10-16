//! Puts a flow's nodes where a person can read them: one row per step from the start, branches side by side.
//! The same layering as the editor's "Tidy" button, so a flow the planner builds looks like one drawn by hand.

use super::model::*;
use std::collections::HashMap;

pub const NODE_W: f64 = 220.0;
pub const NODE_H: f64 = 66.0;

pub fn auto_layout(def: &mut FlowDef) {
    let mut preds: HashMap<&str, Vec<&str>> = def.nodes.iter().map(|n| (n.id.as_str(), Vec::new())).collect();
    for e in &def.edges {
        if preds.contains_key(e.to.as_str()) && preds.contains_key(e.from.as_str()) {
            preds.get_mut(e.to.as_str()).expect("checked").push(e.from.as_str());
        }
    }
    fn depth<'a>(id: &'a str, preds: &HashMap<&'a str, Vec<&'a str>>, memo: &mut HashMap<&'a str, i32>, seen: &mut Vec<&'a str>) -> i32 {
        if let Some(d) = memo.get(id) {
            return *d;
        }
        if seen.contains(&id) {
            return 0; // a cycle: the validator reports it; layout must not loop
        }
        seen.push(id);
        let d = preds.get(id).map(|ps| ps.iter().map(|p| depth(p, preds, memo, seen) + 1).max().unwrap_or(0)).unwrap_or(0);
        seen.pop();
        memo.insert(id, d);
        d
    }
    let mut memo: HashMap<&str, i32> = HashMap::new();
    let ids: Vec<&str> = def.nodes.iter().map(|n| n.id.as_str()).collect();
    let mut levels: Vec<(String, i32)> = Vec::new();
    for id in &ids {
        let d = depth(id, &preds, &mut memo, &mut Vec::new());
        levels.push((id.to_string(), d));
    }
    let mut per_row: HashMap<i32, usize> = HashMap::new();
    for (id, d) in levels {
        let col = per_row.entry(d).or_insert(0);
        if let Some(n) = def.node_mut(&id) {
            n.position = Position { x: 60.0 + *col as f64 * (NODE_W + 60.0), y: 40.0 + d as f64 * (NODE_H + 60.0) };
        }
        *col += 1;
    }
}

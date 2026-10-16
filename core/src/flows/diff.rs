//! What changed between two versions of a flow, in a form a person can read.
//!
//! Used for version comparison and, in Phase 2, to show what the AI intends to change before it is
//! applied. Moving a node on the canvas is reported separately so it does not look like an edit.

use super::model::*;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct NodeChange {
    pub id: String,
    pub name: String,
    /// What differs: "kind", "name", "config.tone", "enabled", ...
    pub fields: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Diff {
    pub nodes_added: Vec<String>,
    pub nodes_removed: Vec<String>,
    pub nodes_changed: Vec<NodeChange>,
    pub nodes_moved: Vec<String>,
    pub edges_added: Vec<String>,
    pub edges_removed: Vec<String>,
    /// Flow-level: "name", "description", "variables", "settings".
    pub flow_changed: Vec<String>,
    /// One short sentence per change.
    pub summary: Vec<String>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.nodes_added.is_empty() && self.nodes_removed.is_empty() && self.nodes_changed.is_empty() && self.edges_added.is_empty() && self.edges_removed.is_empty() && self.flow_changed.is_empty()
    }
    /// Nothing but node positions changed.
    pub fn only_moves(&self) -> bool {
        self.is_empty() && !self.nodes_moved.is_empty()
    }
}

fn label(n: &Node) -> String {
    if n.name.is_empty() {
        n.id.clone()
    } else {
        n.name.clone()
    }
}

/// The settings that differ between two nodes, by name.
fn node_fields(a: &Node, b: &Node) -> Vec<String> {
    let mut out = Vec::new();
    if a.kind != b.kind {
        out.push("kind".to_string());
    }
    if a.name != b.name {
        out.push("name".into());
    }
    if a.description != b.description {
        out.push("description".into());
    }
    if a.enabled != b.enabled {
        out.push("enabled".into());
    }
    if a.retry != b.retry {
        out.push("retry".into());
    }
    if a.on_error != b.on_error {
        out.push("on_error".into());
    }
    if a.credential != b.credential {
        out.push("credential".into());
    }
    if a.inputs != b.inputs {
        out.push("inputs".into());
    }
    if a.outputs != b.outputs {
        out.push("outputs".into());
    }
    let (ca, cb) = (a.config.as_object(), b.config.as_object());
    match (ca, cb) {
        (Some(ca), Some(cb)) => {
            let mut keys: Vec<&String> = ca.keys().chain(cb.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                if ca.get(k) != cb.get(k) {
                    out.push(format!("config.{k}"));
                }
            }
        }
        _ if a.config != b.config => out.push("config".into()),
        _ => {}
    }
    out
}

pub fn diff(a: &FlowDef, b: &FlowDef) -> Diff {
    let mut d = Diff::default();
    let before: HashMap<&str, &Node> = a.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let after: HashMap<&str, &Node> = b.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    for n in &b.nodes {
        match before.get(n.id.as_str()) {
            None => {
                d.nodes_added.push(n.id.clone());
                d.summary.push(format!("Added: {}", label(n)));
            }
            Some(old) => {
                let fields = node_fields(old, n);
                if !fields.is_empty() {
                    d.summary.push(format!("Changed: {} ({})", label(n), fields.join(", ")));
                    d.nodes_changed.push(NodeChange { id: n.id.clone(), name: label(n), fields });
                }
                if old.position != n.position {
                    d.nodes_moved.push(n.id.clone());
                }
            }
        }
    }
    for n in &a.nodes {
        if !after.contains_key(n.id.as_str()) {
            d.nodes_removed.push(n.id.clone());
            d.summary.push(format!("Removed: {}", label(n)));
        }
    }
    let name_of = |def: &FlowDef, id: &str| def.node(id).map(label).unwrap_or_else(|| id.to_string());
    let key = |e: &Edge| (e.from.clone(), e.port.clone(), e.to.clone());
    let old_edges: Vec<_> = a.edges.iter().map(key).collect();
    let new_edges: Vec<_> = b.edges.iter().map(key).collect();
    for e in &b.edges {
        if !old_edges.contains(&key(e)) {
            d.edges_added.push(Edge::make_id(&e.from, &e.port, &e.to));
            d.summary.push(format!("Connected: {} -> {}", name_of(b, &e.from), name_of(b, &e.to)));
        }
    }
    for e in &a.edges {
        if !new_edges.contains(&key(e)) {
            d.edges_removed.push(Edge::make_id(&e.from, &e.port, &e.to));
            d.summary.push(format!("Disconnected: {} -> {}", name_of(a, &e.from), name_of(a, &e.to)));
        }
    }
    if a.name != b.name {
        d.flow_changed.push("name".into());
        d.summary.push(format!("Renamed: {} -> {}", a.name, b.name));
    }
    if a.description != b.description {
        d.flow_changed.push("description".into());
    }
    if a.variables != b.variables {
        d.flow_changed.push("variables".into());
        d.summary.push("Changed the flow's inputs".into());
    }
    if a.settings != b.settings {
        d.flow_changed.push("settings".into());
        d.summary.push("Changed the flow's settings".into());
    }
    d
}

//! The commands that change a flow. The planner (rules or the language model) proposes a list of
//! these; nothing else can change a flow on its behalf. Each command is checked, the whole list is
//! applied to a copy, and the result is validated before anything is saved.
//!
//! Removing a step joins the steps around it, so "remove the image" leaves a working flow, and
//! anything that used the removed step's result is tidied (the sentence that mentioned it goes).

use super::layout::auto_layout;
use super::model::*;
use super::registry;
use super::validate::valid_id;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const MAX_COMMANDS: usize = 300;

fn yes() -> bool {
    true
}
fn out_port() -> String {
    "out".into()
}
fn empty_object() -> Value {
    Value::Object(Map::new())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    pub node: String,
    #[serde(default = "out_port")]
    pub port: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewNode {
    pub id: String,
    pub kind: NodeKind,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "empty_object")]
    pub config: Value,
    #[serde(default)]
    pub credential: Option<String>,
    #[serde(default)]
    pub retry: Option<RetryPolicy>,
    #[serde(default)]
    pub on_error: Option<OnError>,
}

impl NewNode {
    pub fn new(id: &str, kind: NodeKind, name: &str, config: Value) -> Self {
        Self { id: id.into(), kind, name: name.into(), description: String::new(), config, credential: None, retry: None, on_error: None }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    Rename { name: String },
    SetDescription { description: String },
    /// Add a step. With `after`, it is spliced in: it takes over the connections that left that exit.
    /// With `before`, it is spliced in front of a step. With neither, it is added unconnected.
    AddNode {
        node: NewNode,
        #[serde(default)]
        after: Option<Anchor>,
        #[serde(default)]
        before: Option<String>,
    },
    /// Change a step. `config` is a patch: a key set to null is removed.
    UpdateNode {
        id: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        config: Option<Map<String, Value>>,
        #[serde(default)]
        enabled: Option<bool>,
        /// An empty string clears it.
        #[serde(default)]
        credential: Option<String>,
    },
    RemoveNode {
        id: String,
        #[serde(default = "yes")]
        bridge: bool,
    },
    Connect {
        from: String,
        #[serde(default = "out_port")]
        port: String,
        to: String,
    },
    Disconnect {
        from: String,
        #[serde(default = "out_port")]
        port: String,
        to: String,
    },
    AddVariable { variable: VariableDef },
    SetApproval { policy: ApprovalPolicy },
    /// How many times a step tries again by itself, and how long it waits.
    SetRetry { id: String, retries: u32, delay_secs: u64 },
    /// What a step's failure does to the run.
    SetOnError { id: String, on_error: OnError },
    SetTimezone { timezone: String },
    Layout,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    pub def: FlowDef,
    /// Side effects worth telling the person about (a sentence tidied, a step joined).
    pub notes: Vec<String>,
}

fn first_port(kind: NodeKind) -> String {
    registry::spec(kind).out_ports[0].clone()
}

fn label(n: &Node) -> String {
    if n.name.is_empty() {
        n.id.clone()
    } else {
        n.name.clone()
    }
}

fn is_only_token(s: &str) -> bool {
    let t = s.trim();
    t.starts_with("{{") && t.ends_with("}}") && t.matches("{{").count() == 1
}

/// Remove what mentions `{{nodes.<id>.…}}` from one string: the whole value if it is only the
/// reference, otherwise the sentence that contains it. Returns whether anything changed.
fn strip_sentences(s: &str, marker: &str) -> Option<String> {
    if !s.contains(marker) {
        return None;
    }
    if is_only_token(s) {
        return Some(String::new());
    }
    let kept: Vec<&str> = s.split(". ").filter(|part| !part.contains(marker)).collect();
    let mut out = kept.join(". ");
    if out.is_empty() {
        return Some(out);
    }
    if s.trim_end().ends_with('.') && !out.ends_with('.') && kept.len() == s.split(". ").count() {
        out.push('.');
    }
    Some(out)
}

fn scrub(v: &mut Value, marker: &str) -> bool {
    match v {
        Value::String(s) => match strip_sentences(s, marker) {
            Some(new) => {
                *s = new;
                true
            }
            None => false,
        },
        Value::Array(a) => a.iter_mut().fold(false, |acc, x| scrub(x, marker) || acc),
        Value::Object(m) => {
            let mut changed = false;
            let keys: Vec<String> = m.keys().cloned().collect();
            for k in keys {
                let Some(val) = m.get_mut(&k) else { continue };
                if scrub(val, marker) {
                    changed = true;
                    if val.as_str() == Some("") {
                        m.remove(&k);
                    }
                }
            }
            changed
        }
        _ => false,
    }
}

/// After step `removed` is gone, take out what used its results.
fn remove_references(def: &mut FlowDef, removed: &str, notes: &mut Vec<String>) {
    let marker = format!("{{{{nodes.{removed}.");
    for n in &mut def.nodes {
        if scrub(&mut n.config, &marker) {
            notes.push(format!("Took the mention of the removed step \"{removed}\" out of \"{}\"", label(n)));
        }
    }
}

fn check_port(def: &FlowDef, node: &str, port: &str) -> Result<(), String> {
    let n = def.node(node).ok_or_else(|| format!("there is no step \"{node}\""))?;
    let spec = registry::spec(n.kind);
    if spec.out_ports.iter().any(|p| p == port) {
        Ok(())
    } else {
        Err(format!("\"{}\" has no \"{port}\" exit (it has: {})", label(n), spec.out_ports.join(", ")))
    }
}

fn add_edge(def: &mut FlowDef, from: &str, port: &str, to: &str) {
    if from != to && !def.edges.iter().any(|e| e.from == from && e.port == port && e.to == to) {
        def.edges.push(Edge::new(from, to, port));
    }
}

fn apply_one(def: &mut FlowDef, cmd: &Command, notes: &mut Vec<String>) -> Result<(), String> {
    match cmd {
        Command::Rename { name } => {
            if name.trim().is_empty() {
                return Err("a flow needs a name".into());
            }
            def.name = name.trim().to_string();
        }
        Command::SetDescription { description } => def.description = description.trim().to_string(),
        Command::AddNode { node, after, before } => {
            if !valid_id(&node.id) {
                return Err(format!("\"{}\" is not a valid step id", node.id));
            }
            if def.node(&node.id).is_some() {
                return Err(format!("there is already a step \"{}\"", node.id));
            }
            if after.is_some() && before.is_some() {
                return Err("a step can go after one step or before another, not both".into());
            }
            let spec = registry::spec(node.kind);
            let mut n = Node::new(&node.id, node.kind, if node.name.trim().is_empty() { &spec.label } else { node.name.trim() });
            n.config = if node.config.is_null() { empty_object() } else { node.config.clone() };
            n.description = node.description.clone();
            n.credential = node.credential.clone().filter(|c| !c.is_empty());
            if let Some(r) = node.retry {
                n.retry = r;
            }
            if let Some(o) = node.on_error {
                n.on_error = o;
            }
            let lowest = def.nodes.iter().map(|x| x.position.y).fold(0.0_f64, f64::max);
            n.position = Position { x: 60.0, y: lowest + 126.0 };
            let new_first = first_port(node.kind);
            if let Some(a) = after {
                if node.kind == NodeKind::Trigger {
                    return Err("a Trigger starts the flow and cannot go after another step".into());
                }
                check_port(def, &a.node, &a.port)?;
                let moved: Vec<String> = def.edges.iter().filter(|e| e.from == a.node && e.port == a.port).map(|e| e.to.clone()).collect();
                def.edges.retain(|e| !(e.from == a.node && e.port == a.port));
                def.nodes.push(n);
                add_edge(def, &a.node, &a.port, &node.id);
                for t in moved {
                    add_edge(def, &node.id, &new_first, &t);
                }
            } else if let Some(b) = before {
                if node.kind == NodeKind::Trigger {
                    return Err("a Trigger starts the flow and cannot go before another step".into());
                }
                if def.node(b).is_none() {
                    return Err(format!("there is no step \"{b}\""));
                }
                let into: Vec<(String, String)> = def.edges.iter().filter(|e| e.to == *b).map(|e| (e.from.clone(), e.port.clone())).collect();
                def.edges.retain(|e| e.to != *b);
                def.nodes.push(n);
                for (f, p) in into {
                    add_edge(def, &f, &p, &node.id);
                }
                add_edge(def, &node.id, &new_first, b);
            } else {
                def.nodes.push(n);
            }
        }
        Command::UpdateNode { id, name, config, enabled, credential } => {
            let n = def.node_mut(id).ok_or_else(|| format!("there is no step \"{id}\""))?;
            if let Some(name) = name {
                n.name = name.trim().to_string();
            }
            if let Some(patch) = config {
                let mut cfg = n.config.as_object().cloned().unwrap_or_default();
                for (k, v) in patch {
                    if v.is_null() {
                        cfg.remove(k);
                    } else {
                        cfg.insert(k.clone(), v.clone());
                    }
                }
                n.config = Value::Object(cfg);
            }
            if let Some(e) = enabled {
                n.enabled = *e;
            }
            if let Some(c) = credential {
                n.credential = if c.is_empty() { None } else { Some(c.clone()) };
            }
        }
        Command::RemoveNode { id, bridge } => {
            let node = def.node(id).ok_or_else(|| format!("there is no step \"{id}\""))?.clone();
            if node.kind == NodeKind::Trigger && def.nodes.iter().filter(|n| n.kind == NodeKind::Trigger).count() == 1 {
                return Err("the flow's only Trigger cannot be removed".into());
            }
            let port = first_port(node.kind);
            let into: Vec<(String, String)> = def.edges.iter().filter(|e| e.to == *id).map(|e| (e.from.clone(), e.port.clone())).collect();
            let onward: Vec<String> = def.edges.iter().filter(|e| e.from == *id && e.port == port).map(|e| e.to.clone()).collect();
            def.edges.retain(|e| e.from != *id && e.to != *id);
            def.nodes.retain(|n| n.id != *id);
            if *bridge {
                for (f, p) in &into {
                    for t in &onward {
                        add_edge(def, f, p, t);
                    }
                }
                if !into.is_empty() && !onward.is_empty() {
                    notes.push(format!("Joined the steps around \"{}\" so the flow still connects", label(&node)));
                }
            }
            remove_references(def, id, notes);
        }
        Command::Connect { from, port, to } => {
            check_port(def, from, port)?;
            let target = def.node(to).ok_or_else(|| format!("there is no step \"{to}\""))?;
            if target.kind == NodeKind::Trigger {
                return Err("nothing can lead into a Trigger".into());
            }
            if from == to {
                return Err("a step cannot be connected to itself".into());
            }
            add_edge(def, from, port, to);
        }
        Command::Disconnect { from, port, to } => {
            let before = def.edges.len();
            def.edges.retain(|e| !(e.from == *from && e.port == *port && e.to == *to));
            if def.edges.len() == before {
                return Err(format!("there is no connection {from} -> {to}"));
            }
        }
        Command::AddVariable { variable } => {
            if def.variables.iter().any(|v| v.name == variable.name) {
                return Err(format!("there is already an input \"{}\"", variable.name));
            }
            def.variables.push(variable.clone());
        }
        Command::SetApproval { policy } => def.settings.approval = *policy,
        Command::SetRetry { id, retries, delay_secs } => {
            if *retries > 10 || *delay_secs > 300 {
                return Err("a step may retry at most 10 times, waiting at most 5 minutes".into());
            }
            let n = def.node_mut(id).ok_or_else(|| format!("there is no step \"{id}\""))?;
            n.retry.retries = *retries;
            n.retry.delay_secs = (*delay_secs).max(1);
        }
        Command::SetOnError { id, on_error } => {
            def.node_mut(id).ok_or_else(|| format!("there is no step \"{id}\""))?.on_error = *on_error;
        }
        Command::SetTimezone { timezone } => def.settings.timezone = timezone.trim().to_string(),
        Command::Layout => auto_layout(def),
    }
    Ok(())
}

/// Apply every command to a copy of `base`. All or nothing: the first command that cannot be applied
/// stops the list and names the command and the reason.
pub fn apply(base: &FlowDef, commands: &[Command]) -> Result<Applied, String> {
    if commands.len() > MAX_COMMANDS {
        return Err(format!("that is {} changes; at most {MAX_COMMANDS} at a time", commands.len()));
    }
    let mut def = base.clone();
    let mut notes = Vec::new();
    for (i, c) in commands.iter().enumerate() {
        apply_one(&mut def, c, &mut notes).map_err(|e| format!("change {} could not be applied: {e}", i + 1))?;
    }
    def.normalize();
    def.check_size()?;
    Ok(Applied { def, notes })
}

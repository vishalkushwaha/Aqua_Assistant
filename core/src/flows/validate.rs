//! Checks a flow before it can run, and tells the canvas which node to mark.
//!
//! Errors stop a run. Warnings do not, but are worth reading. Info is for things the person should
//! know (a step will ask before it runs). Saving a draft never needs a clean report; running does.

use super::looping;
use super::model::*;
use super::registry::{self, FieldKind};
use crate::workflows::model::secret_in;
use crate::workflows::schedule::CronExpr;
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Issue {
    pub severity: Severity,
    /// The node it is about, if any.
    pub node: Option<String>,
    /// A stable code, for tests and for the UI to pick an icon.
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Report {
    pub issues: Vec<Issue>,
    /// Nodes that will ask a person before they run (high-impact and no approval step before them).
    pub needs_approval: Vec<String>,
}

impl Report {
    pub fn errors(&self) -> impl Iterator<Item = &Issue> {
        self.issues.iter().filter(|i| i.severity == Severity::Error)
    }
    pub fn ok(&self) -> bool {
        self.errors().next().is_none()
    }
    pub fn has(&self, code: &str) -> bool {
        self.issues.iter().any(|i| i.code == code)
    }
    pub fn for_node(&self, id: &str) -> Vec<&Issue> {
        self.issues.iter().filter(|i| i.node.as_deref() == Some(id)).collect()
    }
    fn add(&mut self, severity: Severity, node: Option<&str>, code: &str, message: impl Into<String>) {
        self.issues.push(Issue { severity, node: node.map(String::from), code: code.into(), message: message.into() });
    }
    fn error(&mut self, node: Option<&str>, code: &str, message: impl Into<String>) {
        self.add(Severity::Error, node, code, message);
    }
    fn warn(&mut self, node: Option<&str>, code: &str, message: impl Into<String>) {
        self.add(Severity::Warning, node, code, message);
    }
    fn info(&mut self, node: Option<&str>, code: &str, message: impl Into<String>) {
        self.add(Severity::Info, node, code, message);
    }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 48 && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Every `{{...}}` path in the strings of a JSON value (before any `|` filter).
pub fn references(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            let mut rest = s.as_str();
            while let Some(i) = rest.find("{{") {
                let after = &rest[i + 2..];
                let Some(end) = after.find("}}") else { break };
                let inner = after[..end].split('|').next().unwrap_or("").trim();
                if !inner.is_empty() {
                    out.push(inner.to_string());
                }
                rest = &after[end + 2..];
            }
        }
        Value::Array(a) => a.iter().for_each(|x| references(x, out)),
        Value::Object(m) => m.values().for_each(|x| references(x, out)),
        _ => {}
    }
}

fn is_template(v: &Value) -> bool {
    v.as_str().is_some_and(|s| s.contains("{{"))
}

fn empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        _ => false,
    }
}

fn check_config(report: &mut Report, n: &Node) {
    let spec = registry::spec(n.kind);
    let Some(cfg) = n.config.as_object() else {
        report.error(Some(&n.id), "bad_config", "the settings must be an object");
        return;
    };
    for field in &spec.fields {
        let v = cfg.get(&field.name);
        let missing = v.is_none_or(empty);
        if missing {
            if field.required && field.default.is_none() {
                report.error(Some(&n.id), "missing_config", format!("{}: \"{}\" is required", n.name, field.label));
            }
            continue;
        }
        let v = v.expect("checked above");
        // The words of a condition and of a `{{ }}` are checked here, so a mistake is found when the flow is saved and not when a run reaches it.
        if let Some(text) = v.as_str() {
            if n.kind == NodeKind::Condition && field.name == "expression" {
                if let Err(e) = crate::workflows::template::check_condition(text) {
                    report.error(Some(&n.id), "bad_condition", format!("{}: \"{}\" {e}", n.name, field.label));
                }
                continue;
            }
            // The query of a Database step is never filled in from `{{ }}`: a value reaches it only as one of the step's "Values" (card R7-12).
            if n.kind == NodeKind::Database && field.name == "query" {
                if text.contains("{{") {
                    report.error(Some(&n.id), "sql_template", format!("{}: the query cannot have {{{{ }}}} in it, so that text from a page or a step can never become part of a query. Write ? where a value goes and give the value under \"Values\"", n.name));
                }
                continue;
            }
            // A list written out in a For each step is read the way the engine will read it.
            if n.kind == NodeKind::Loop && field.name == "items" && !text.contains("{{") {
                match looping::list_from_text(text) {
                    Err(e) => report.error(Some(&n.id), "bad_list", format!("{}: {e}", n.name)),
                    Ok(list) if list.len() > looping::MAX_ITEMS => report.error(Some(&n.id), "too_many_items", format!("{}: the list has {} items, and a For each step runs at most {}. Make the list shorter or split the work between two flows", n.name, list.len(), looping::MAX_ITEMS)),
                    Ok(_) => {}
                }
                continue;
            }
            if is_template(v) {
                if let Err(e) = crate::workflows::template::check_template(text) {
                    report.warn(Some(&n.id), "bad_template", format!("{}: \"{}\" {e}", n.name, field.label));
                }
            }
        }
        if is_template(v) {
            continue; // a reference: checked by the reference pass
        }
        let bad = |report: &mut Report, why: &str| report.error(Some(&n.id), "bad_config", format!("{}: \"{}\" {why}", n.name, field.label));
        match field.kind {
            FieldKind::Number if !v.is_number() => bad(report, "must be a number"),
            FieldKind::Bool if !v.is_boolean() => bad(report, "must be true or false"),
            FieldKind::Choice if !v.as_str().is_some_and(|s| field.choices.iter().any(|c| c == s)) => bad(report, &format!("must be one of: {}", field.choices.join(", "))),
            FieldKind::Url if !v.as_str().is_some_and(|s| s.starts_with("http://") || s.starts_with("https://")) => bad(report, "must be a web address starting with http:// or https://"),
            FieldKind::Json if !(v.is_object() || v.is_array() || v.as_str().is_some_and(|s| serde_json::from_str::<Value>(s).is_ok())) => bad(report, "must be valid JSON"),
            FieldKind::Text | FieldKind::LongText if !v.is_string() && !v.is_number() => bad(report, "must be text"),
            _ => {}
        }
    }
    for key in cfg.keys() {
        if !spec.fields.iter().any(|f| &f.name == key) {
            report.warn(Some(&n.id), "unknown_config", format!("{}: \"{key}\" is not a setting of this kind of step and is ignored", n.name));
        }
    }
    if let Some(path) = secret_in(&n.config, "settings") {
        report.error(Some(&n.id), "secret_in_config", format!("{}: {path} looks like a password or token. Do not store secrets in a flow: name a credential on the step instead.", n.name));
    }
    if let Some(name) = &n.credential {
        if !aqua_native_tools::secrets::valid_name(name) {
            report.error(Some(&n.id), "bad_credential", format!("{}: \"{name}\" is not a valid credential name", n.name));
        }
    }
    if n.kind == NodeKind::Loop && cfg.get("items").is_some_and(|v| v.is_number()) {
        report.error(Some(&n.id), "bad_list", format!("{}: \"List\" is a number, not a list", n.name));
    }
    if n.kind == NodeKind::Database {
        check_database(report, n, cfg);
    }
    if n.kind == NodeKind::SocialMedia {
        let plat = cfg.get("platform").and_then(|v| v.as_str()).and_then(super::platform::get);
        let action = cfg.get("action").and_then(|v| v.as_str());
        if let (Some(p), Some(a)) = (plat, action) {
            let available = super::platform::available_actions(p);
            if !available.contains(&a) {
                report.error(Some(&n.id), "unsupported_action", format!("{}: {} cannot \"{a}\" yet. What it can do: {}", n.name, p.label, available.join(", ")));
            }
            if let Some(why) = super::platform::not_built_note(p) {
                report.warn(Some(&n.id), "platform_not_built", format!("{}: {why}, so a real run of this flow is refused before it starts (a dry run works). Change the platform to one that works, or switch this step off", n.name));
            }
            if cfg.get("mode").and_then(|v| v.as_str()) == Some("connector") && super::platform::connector_actions(p).is_empty() {
                report.error(Some(&n.id), "no_connector", format!("{}: no connector is set up for {}. Use \"auto\" or \"browser\".", n.name, p.label));
            }
        }
    }
    if n.kind == NodeKind::Schedule && cfg.get("repeat").and_then(|v| v.as_str()).unwrap_or("once") == "once" {
        if cfg.get("when").is_none_or(empty) {
            report.error(Some(&n.id), "missing_config", format!("{}: a one-time schedule needs a date and time", n.name));
        }
        if let Some(when) = cfg.get("when").and_then(|v| v.as_str()).filter(|w| !w.contains("{{")) {
            if chrono::NaiveDateTime::parse_from_str(when, "%Y-%m-%d %H:%M").is_err() {
                report.error(Some(&n.id), "bad_schedule", format!("{}: \"{when}\" is not a date and time; write it like 2026-10-01 10:00", n.name));
            }
        }
    }
    if n.kind == NodeKind::Trigger && cfg.get("trigger_type").and_then(|v| v.as_str()) == Some("schedule") {
        match cfg.get("cron").and_then(|v| v.as_str()) {
            Some(c) => {
                if let Err(e) = CronExpr::parse(c) {
                    report.error(Some(&n.id), "bad_schedule", format!("{}: the schedule is not valid: {e}", n.name));
                }
            }
            None => report.error(Some(&n.id), "missing_config", format!("{}: a scheduled trigger needs a schedule", n.name)),
        }
    }
    if n.kind == NodeKind::Schedule && cfg.get("repeat").and_then(|v| v.as_str()) == Some("cron") {
        if let Some(Err(e)) = cfg.get("cron").and_then(|v| v.as_str()).map(CronExpr::parse) {
            report.error(Some(&n.id), "bad_schedule", format!("{}: the schedule is not valid: {e}", n.name));
        }
    }
    check_promises(report, n, cfg);
}

/// The first word of a SQL text, in lower case, after any space and comments.
fn first_sql_word(sql: &str) -> String {
    let mut rest = sql.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map(|(_, r)| r).unwrap_or("").trim_start();
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map(|(_, r)| r).unwrap_or("").trim_start();
        } else {
            break;
        }
    }
    rest.chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_lowercase()
}

/// The Database step reads and nothing else: a query that starts with a write is found here, before any step of the run has happened, and not when the tool refuses it. The tool
/// is the judge of what is a read (card R7-12); this finds the plain cases early. (`params` is checked too: a list of plain values.)
fn check_database(report: &mut Report, n: &Node, cfg: &serde_json::Map<String, Value>) {
    if let Some(sql) = cfg.get("query").and_then(|v| v.as_str()).filter(|q| !q.contains("{{")) {
        let word = first_sql_word(sql);
        if matches!(word.as_str(), "insert" | "update" | "delete" | "replace" | "drop" | "create" | "alter" | "truncate" | "attach" | "detach" | "pragma" | "vacuum" | "grant" | "revoke" | "merge" | "call" | "exec" | "execute") {
            report.error(Some(&n.id), "db_not_a_read", format!("{}: this step only reads. It runs SELECT, WITH, VALUES or EXPLAIN, and \"{}\" would be refused; changing data is not available in a flow yet", n.name, word.to_uppercase()));
        }
    }
    if let Some(p) = cfg.get("params").filter(|p| !empty(p) && !is_template(p)) {
        let list = match p {
            Value::Array(a) => Some(a.clone()),
            Value::String(s) => serde_json::from_str::<Value>(s).ok().and_then(|v| v.as_array().cloned()),
            _ => None,
        };
        match list {
            None => report.error(Some(&n.id), "bad_config", format!("{}: \"Values\" must be a JSON list, like [\"a\", 5]", n.name)),
            Some(a) if a.iter().any(|x| x.is_array() || x.is_object()) => report.error(Some(&n.id), "bad_config", format!("{}: each of the \"Values\" must be text, a number, true/false or empty, not a list or a record", n.name)),
            Some(_) => {}
        }
    }
}

/// Card R6-14 (T6-14.2, T6-14.7): what a step says it will do and nothing does yet. These are warnings and an info, never errors: the flow can be saved, and run by hand.
fn check_promises(report: &mut Report, n: &Node, cfg: &serde_json::Map<String, Value>) {
    if n.kind == NodeKind::Trigger {
        match cfg.get("trigger_type").and_then(|v| v.as_str()) {
            Some(kind @ ("webhook" | "startup" | "event")) => {
                let what = match kind {
                    "webhook" => "a webhook calling",
                    "startup" => "Aqua starting",
                    _ => "an event",
                };
                report.warn(
                    Some(&n.id),
                    "trigger_not_built",
                    format!("{}: nothing starts a flow from {what} yet, so this flow starts only when you run it by hand or when a schedule is made for it", n.name),
                );
            }
            Some("schedule") => report.info(
                Some(&n.id),
                "trigger_needs_a_schedule",
                format!("{}: a flow runs on a time only when a schedule is made for it (under Schedules); this setting alone starts nothing", n.name),
            ),
            _ => {}
        }
    }
    if n.kind == NodeKind::Notification {
        let channel = cfg.get("channel").and_then(|v| v.as_str()).unwrap_or("in_app");
        if !channel.contains("{{") && channel != "in_app" {
            report.warn(
                Some(&n.id),
                "notification_channel_not_connected",
                format!("{}: a Notify step shows its message in the run's log only; it cannot send it by {channel} yet, so nothing will reach you there", n.name),
            );
        }
    }
}

/// For each node, the set of nodes that can reach it (its ancestors).
fn ancestors(def: &FlowDef, ids: &HashSet<&str>) -> HashMap<String, HashSet<String>> {
    let mut back: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in &def.edges {
        if ids.contains(e.from.as_str()) && ids.contains(e.to.as_str()) {
            back.entry(e.to.as_str()).or_default().push(e.from.as_str());
        }
    }
    let mut out = HashMap::new();
    for n in &def.nodes {
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<&str> = VecDeque::from([n.id.as_str()]);
        while let Some(cur) = queue.pop_front() {
            for &p in back.get(cur).map(Vec::as_slice).unwrap_or(&[]) {
                if seen.insert(p.to_string()) {
                    queue.push_back(p);
                }
            }
        }
        out.insert(n.id.clone(), seen);
    }
    out
}

pub fn validate(def: &FlowDef) -> Report {
    let mut r = Report::default();
    if def.name.trim().is_empty() {
        r.error(None, "no_name", "the flow needs a name");
    }
    if let Err(e) = def.check_size() {
        r.error(None, "too_large", e);
        return r;
    }
    if def.nodes.is_empty() {
        r.error(None, "no_nodes", "the flow has no steps");
        return r;
    }

    // ---- ids and edges ----------------------------------------------------------------------
    let mut ids: HashSet<&str> = HashSet::new();
    for n in &def.nodes {
        if !valid_id(&n.id) {
            r.error(Some(&n.id), "bad_id", format!("\"{}\": an id is 1 to 48 characters of a-z, 0-9, _ or -", n.id));
        }
        if !ids.insert(n.id.as_str()) {
            r.error(Some(&n.id), "duplicate_id", format!("two steps have the id \"{}\"", n.id));
        }
    }
    let mut seen_edges: HashSet<(&str, &str, &str)> = HashSet::new();
    let mut good_edges: Vec<&Edge> = Vec::new();
    for e in &def.edges {
        let (Some(from), Some(to)) = (def.node(&e.from), def.node(&e.to)) else {
            r.error(None, "dangling_edge", format!("a connection goes {} -> {}, and one end does not exist", e.from, e.to));
            continue;
        };
        if e.from == e.to {
            r.error(Some(&e.from), "self_loop", format!("{} is connected to itself", from.name));
            continue;
        }
        let spec = registry::spec(from.kind);
        if !spec.out_ports.iter().any(|p| p == &e.port) {
            r.error(Some(&e.from), "bad_port", format!("{} has no \"{}\" exit (it has: {})", from.name, e.port, spec.out_ports.join(", ")));
            continue;
        }
        if to.kind == NodeKind::Trigger {
            r.error(Some(&e.to), "trigger_has_input", format!("{} starts the flow and cannot have anything before it", to.name));
            continue;
        }
        if !seen_edges.insert((e.from.as_str(), e.port.as_str(), e.to.as_str())) {
            r.warn(Some(&e.from), "duplicate_edge", format!("{} -> {} is connected twice", from.name, to.name));
            continue;
        }
        good_edges.push(e);
    }

    // ---- triggers ---------------------------------------------------------------------------
    let triggers: Vec<&Node> = def.nodes.iter().filter(|n| n.kind == NodeKind::Trigger && n.enabled).collect();
    if triggers.is_empty() {
        r.error(None, "no_trigger", "the flow needs a Trigger step to start from");
    }

    // ---- cycles (Kahn) ----------------------------------------------------------------------
    let mut indeg: HashMap<&str, usize> = def.nodes.iter().map(|n| (n.id.as_str(), 0)).collect();
    for e in &good_edges {
        *indeg.entry(e.to.as_str()).or_default() += 1;
    }
    let mut queue: VecDeque<&str> = indeg.iter().filter(|(_, d)| **d == 0).map(|(k, _)| *k).collect();
    let mut visited = 0;
    while let Some(cur) = queue.pop_front() {
        visited += 1;
        for e in good_edges.iter().filter(|e| e.from == cur) {
            let d = indeg.get_mut(e.to.as_str()).expect("edge ends exist");
            *d -= 1;
            if *d == 0 {
                queue.push_back(e.to.as_str());
            }
        }
    }
    if visited < def.nodes.len() {
        let mut stuck: Vec<&str> = indeg.iter().filter(|(_, d)| **d > 0).map(|(k, _)| *k).collect();
        stuck.sort();
        r.error(stuck.first().copied(), "cycle", format!("the steps {} form a loop; a flow cannot go back to an earlier step (use a For each step to repeat work)", stuck.join(", ")));
    }

    // ---- reachability through enabled steps -----------------------------------------------------
    let mut reach: HashSet<&str> = HashSet::new();
    let mut q: VecDeque<&str> = triggers.iter().map(|t| t.id.as_str()).collect();
    for t in &triggers {
        reach.insert(t.id.as_str());
    }
    while let Some(cur) = q.pop_front() {
        for e in good_edges.iter().filter(|e| e.from == cur) {
            // A switched-off step is skipped, and the flow carries on past it.
            if reach.insert(e.to.as_str()) {
                q.push_back(e.to.as_str());
            }
        }
    }
    for n in def.nodes.iter().filter(|n| n.enabled && !reach.contains(n.id.as_str())) {
        r.error(Some(&n.id), "unreachable", format!("{} can never run: nothing connects it to the start", n.name));
    }

    // ---- variables ---------------------------------------------------------------------------
    let mut var_names: HashSet<String> = HashSet::new();
    for v in &def.variables {
        if !valid_id(&v.name.replace('.', "-")) {
            r.error(None, "bad_variable", format!("the input \"{}\" needs a name of a-z, 0-9, _ or -", v.name));
        }
        if !var_names.insert(v.name.clone()) {
            r.error(None, "duplicate_variable", format!("two inputs are called \"{}\"", v.name));
        }
        if v.kind == VarKind::Choice && v.choices.is_empty() {
            r.error(None, "bad_variable", format!("the input \"{}\" is a choice but has no choices", v.name));
        }
    }
    for n in def.nodes.iter().filter(|n| n.kind == NodeKind::SetVariable) {
        if let Some(name) = n.config.get("name").and_then(|v| v.as_str()) {
            var_names.insert(name.to_string());
        }
    }

    // Names that start with two underscores are Aqua's own (the run's record of which variables hold text from outside lives under one): a flow cannot use them.
    for name in var_names.iter().filter(|n| n.starts_with("__")) {
        r.error(None, "reserved_variable", format!("\"{name}\" cannot be the name of an input or of a value: names that start with two underscores are Aqua's own"));
    }

    // ---- per-node configuration and references -----------------------------------------------
    let anc = ancestors(def, &ids);
    for n in &def.nodes {
        check_config(&mut r, n);
        let mut refs = Vec::new();
        // The query of a Database step is not filled in (and is refused if it tries): its `{{ }}` are not references.
        if n.kind == NodeKind::Database {
            let mut without_query = n.config.clone();
            if let Some(o) = without_query.as_object_mut() {
                o.remove("query");
            }
            references(&without_query, &mut refs);
        } else {
            references(&n.config, &mut refs);
        }
        for path in refs {
            let parts: Vec<&str> = path.split('.').collect();
            match parts.first().copied() {
                Some("nodes") => {
                    let (Some(id), Some(out)) = (parts.get(1), parts.get(2)) else {
                        r.error(Some(&n.id), "bad_reference", format!("{}: {{{{{path}}}}} should look like nodes.<step>.<value>", n.name));
                        continue;
                    };
                    // `results[*]` and `rows[0]` name the value `results` and `rows`: what follows the bracket looks inside it.
                    let out = &out.split('[').next().unwrap_or("");
                    let Some(target) = def.node(id) else {
                        r.error(Some(&n.id), "missing_node_ref", format!("{}: {{{{{path}}}}} points at a step that does not exist", n.name));
                        continue;
                    };
                    if *id == n.id || !anc.get(&n.id).is_some_and(|a| a.contains(*id)) {
                        // A step inside a For each step is read from outside it through the loop's results.
                        let hint = match looping::enclosing_loop(def, id) {
                            Some(l) if looping::enclosing_loop(def, &n.id) != Some(l) => format!(" It is inside the For each step \"{}\": after that step, read {{{{nodes.{l}.results}}}} instead", def.node(l).map(|x| x.name.as_str()).unwrap_or(l)),
                            _ => String::new(),
                        };
                        r.error(Some(&n.id), "not_upstream", format!("{}: {{{{{path}}}}} uses the result of {}, which does not run before this step.{hint}", n.name, target.name));
                    } else if !matches!(*out, "ok" | "error") && !target.outputs.iter().any(|o| o == out) && !registry::spec(target.kind).default_outputs.iter().any(|o| o == out) {
                        r.error(Some(&n.id), "missing_output", format!("{}: {} does not produce \"{out}\" (it produces: {})", n.name, target.name, target.outputs.join(", ")));
                    } else if !target.enabled {
                        r.warn(Some(&n.id), "disabled_ref", format!("{}: uses {}, which is switched off, so the value will be empty", n.name, target.name));
                    }
                }
                Some("vars") => {
                    let name = parts.get(1).copied().unwrap_or("");
                    if !var_names.contains(name) {
                        r.error(Some(&n.id), "undefined_variable", format!("{}: {{{{{path}}}}} uses an input that is not defined", n.name));
                    }
                }
                Some("trigger" | "now" | "flow" | "run") => {}
                _ => r.error(Some(&n.id), "unknown_reference", format!("{}: {{{{{path}}}}} is not something a flow can refer to", n.name)),
            }
        }
    }

    // ---- steps nothing can run yet --------------------------------------------------------------
    for n in def.nodes.iter().filter(|n| n.enabled && !registry::spec(n.kind).runnable) {
        r.warn(Some(&n.id), "not_runnable", format!("{}: this step cannot run yet ({}). The flow can be saved, but a run that reaches it is refused before it starts", n.name, registry::spec(n.kind).label));
    }

    // ---- branches ----------------------------------------------------------------------------
    for n in def.nodes.iter().filter(|n| n.enabled) {
        let has = |port: &str| good_edges.iter().any(|e| e.from == n.id && e.port == port);
        match n.kind {
            NodeKind::Condition => {
                for port in ["true", "false"] {
                    if !has(port) {
                        r.warn(Some(&n.id), "open_branch", format!("{}: nothing is connected to the \"{port}\" exit, so the flow ends there", n.name));
                    }
                }
            }
            NodeKind::Approval => {
                if !has("approved") {
                    r.warn(Some(&n.id), "open_branch", format!("{}: nothing follows an approval", n.name));
                }
                if !has("rejected") {
                    r.info(Some(&n.id), "open_branch", format!("{}: if you reject, the flow ends there", n.name));
                }
            }
            NodeKind::Loop if !has("each") => r.warn(Some(&n.id), "open_branch", format!("{}: nothing is connected to \"each\"", n.name)),
            _ => {}
        }
    }

    // ---- For each: what is inside it ---------------------------------------------------------------
    // The steps inside a For each step run once for each item, so they connect only to each other (and the first of them to the "each" exit): a step outside could not say which
    // item it belongs to. What the loop made is read after it, through its "done" exit and `{{nodes.<loop>.results}}`. A loop inside a loop is not supported.
    for l in def.nodes.iter().filter(|n| n.enabled && n.kind == NodeKind::Loop) {
        let inside_ids = looping::body_of(def, &l.id);
        let inside: HashSet<&str> = inside_ids.iter().map(String::as_str).collect();
        for b in def.nodes.iter().filter(|n| inside.contains(n.id.as_str())) {
            if b.kind == NodeKind::Loop {
                r.error(Some(&b.id), "nested_loop", format!("{}: a For each step inside another For each step (\"{}\") is not supported yet. Do the inner work in a flow of its own, or make one list of the two", b.name, l.name));
            }
            // Whatever is reached from "each" is inside, so a step outside can only be joined to a step inside by a connection that comes in: from the loop's "done" exit, or from outside.
            for e in def.edges.iter().filter(|e| e.to == b.id && def.node(&e.from).is_some()) {
                if (e.from == l.id && e.port == "each") || inside.contains(e.from.as_str()) {
                    continue;
                }
                let from = def.node(&e.from).map(|x| x.name.clone()).unwrap_or_default();
                if e.from == l.id {
                    r.error(Some(&b.id), "loop_body_open", format!("{}: it is inside the For each step \"{from}\" (it can be reached from \"each\") but is also connected from its \"{}\" exit. What follows the loop connects to \"done\" and is not also connected to a step inside, which reads {{{{nodes.{}.results}}}} instead", b.name, e.port, l.id));
                } else {
                    r.error(Some(&b.id), "loop_body_open", format!("{}: it is inside the For each step \"{}\" but is also connected from \"{from}\", which is outside it. Steps inside a For each step connect only to each other", b.name, l.name));
                }
            }
        }
    }

    // ---- permissions --------------------------------------------------------------------------
    for n in def.nodes.iter().filter(|n| n.enabled && registry::high_impact(n)) {
        let gated = anc.get(&n.id).is_some_and(|a| a.iter().any(|id| def.node(id).is_some_and(|x| x.kind == NodeKind::Approval && x.enabled)));
        if gated {
            continue;
        }
        if def.settings.approval == ApprovalPolicy::Never {
            r.warn(Some(&n.id), "no_approval", format!("{} has an effect outside Aqua and will run without asking, because approvals are switched off for this flow", n.name));
        } else {
            r.needs_approval.push(n.id.clone());
            // Inside a For each step it is once for every item, unless an Approval step comes before the loop.
            let each = if looping::enclosing_loop(def, &n.id).is_some() { ", for every item of the list; an Approval step before the For each step asks once for all of them" } else { "" };
            r.info(Some(&n.id), "will_ask", format!("{} has an effect outside Aqua; Aqua will ask you before it runs{each}", n.name));
        }
    }
    r
}

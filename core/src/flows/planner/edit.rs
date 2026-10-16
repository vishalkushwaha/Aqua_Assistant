//! Changes to an existing flow, from a sentence: "Switch LinkedIn to Instagram", "Remove the image
//! step", "Add an approval step before publishing", "Schedule it for tomorrow at 10 AM".
//!
//! A request may hold several changes ("remove the image and add research"). Each clause is matched
//! against a rule; the rules produce commands, and the commands are applied to a working copy before
//! the next clause is read, so a later clause sees what an earlier one did. A clause no rule matches
//! is reported, never guessed at. Only what the request names is touched; the rest of the flow is kept.

use super::compose::Answers;
use super::intent::words_lower;
use super::Question;
use crate::flows::commands::{apply, Anchor, Command, NewNode};
use crate::flows::model::*;
use crate::flows::platform::{self, Platform};
use crate::flows::schedule_text::{self, Repeat};
use chrono::NaiveDateTime;
use serde_json::{json, Map};

#[derive(Debug, Default)]
pub struct EditPlan {
    pub commands: Vec<Command>,
    pub explanation: Vec<String>,
    pub questions: Vec<Question>,
    pub warnings: Vec<String>,
    /// Clauses no rule understood.
    pub not_understood: Vec<String>,
    pub schedule: Option<super::ScheduleSuggestion>,
}

#[derive(Default)]
struct Out {
    schedule: Option<super::ScheduleSuggestion>,
    commands: Vec<Command>,
    explain: Vec<String>,
    warnings: Vec<String>,
    questions: Vec<Question>,
}

impl Out {
    fn say(text: impl Into<String>) -> Self {
        Out { explain: vec![text.into()], ..Default::default() }
    }
}

const VERBS: &[&str] = &["remove", "delete", "drop", "add", "include", "enable", "disable", "switch", "change", "replace", "swap", "schedule", "save", "skip", "use"];

fn has(w: &[String], any: &[&str]) -> bool {
    w.iter().any(|x| any.contains(&x.as_str()))
}

fn has_phrase(joined: &str, any: &[&str]) -> bool {
    any.iter().any(|p| joined.contains(p))
}

fn label(n: &Node) -> String {
    if n.name.is_empty() {
        n.id.clone()
    } else {
        n.name.clone()
    }
}

fn replace_ci(text: &str, from: &str, to: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let needle = from.to_ascii_lowercase();
    let mut out = String::new();
    let mut i = 0;
    while let Some(pos) = lower[i..].find(&needle) {
        out.push_str(&text[i..i + pos]);
        out.push_str(to);
        i += pos + needle.len();
    }
    out.push_str(&text[i..]);
    out
}

fn replace_platform_words(text: &str, from: &Platform, to: &Platform) -> String {
    let mut s = replace_ci(text, from.label, to.label);
    for a in from.aliases {
        if a.len() > 2 {
            s = replace_ci(&s, a, to.label);
        }
    }
    s
}

fn mentions(text: &str, p: &Platform) -> bool {
    let l = text.to_ascii_lowercase();
    l.contains(&p.label.to_ascii_lowercase()) || p.aliases.iter().any(|a| a.len() > 2 && l.contains(a))
}

/// The platforms the flow publishes to.
fn flow_platforms(def: &FlowDef) -> Vec<&'static Platform> {
    let mut v: Vec<&'static Platform> = Vec::new();
    for n in def.nodes.iter().filter(|n| n.kind == NodeKind::SocialMedia) {
        if let Some(p) = n.config.get("platform").and_then(|x| x.as_str()).and_then(platform::get) {
            if !v.iter().any(|x| x.id == p.id) {
                v.push(p);
            }
        }
    }
    v
}

fn of_kind(def: &FlowDef, kind: NodeKind) -> Vec<&Node> {
    def.nodes.iter().filter(|n| n.kind == kind).collect()
}

/// The step that writes the post: a text step whose id or name says "write", else the first one.
fn writer(def: &FlowDef) -> Option<&Node> {
    let texts = of_kind(def, NodeKind::TextGeneration);
    texts.iter().copied().find(|n| n.id.contains("write") || label(n).to_ascii_lowercase().contains("write")).or_else(|| texts.first().copied())
}

fn is_image_step(n: &Node) -> bool {
    n.kind == NodeKind::ImageGeneration || (n.kind == NodeKind::TextGeneration && (n.id.contains("image") || label(n).to_ascii_lowercase().contains("image")))
}

fn text_ref(def: &FlowDef) -> String {
    writer(def).map(|w| format!("{{{{nodes.{}.text}}}}", w.id)).unwrap_or_default()
}

fn unique_id(def: &FlowDef, base: &str) -> String {
    if def.node(base).is_none() {
        return base.to_string();
    }
    (2..).map(|i| format!("{base}_{i}")).find(|c| def.node(c).is_none()).expect("an unused id exists")
}

// ---- finding steps by what a person calls them ---------------------------------------------------

const STOP: &[&str] = &["the", "a", "an", "step", "steps", "node", "nodes", "please", "it", "all", "of", "this", "that", "from", "flow", "generation", "generate", "generating", "my", "our", "again", "too", "also", "any", "and", "to", "in", "out", "phase"];

fn things(def: &FlowDef, phrase_words: &[String]) -> Vec<String> {
    let w = |a: &[&str]| has(phrase_words, a);
    if w(&["image", "images", "picture", "photo", "illustration", "graphic", "visual"]) {
        return def.nodes.iter().filter(|n| is_image_step(n)).map(|n| n.id.clone()).collect();
    }
    if w(&["research", "researching", "sources"]) {
        return of_kind(def, NodeKind::Research).iter().map(|n| n.id.clone()).collect();
    }
    if w(&["approval", "approve", "confirmation"]) {
        return of_kind(def, NodeKind::Approval).iter().map(|n| n.id.clone()).collect();
    }
    if w(&["excel", "spreadsheet", "xlsx"]) {
        return def.nodes.iter().filter(|n| n.kind == NodeKind::Excel || (matches!(n.kind, NodeKind::UserInput | NodeKind::Condition) && (n.id.contains("excel") || label(n).to_ascii_lowercase().contains("excel")))).map(|n| n.id.clone()).collect();
    }
    if w(&["schedule", "scheduling", "delay"]) {
        return def.nodes.iter().filter(|n| n.kind == NodeKind::Schedule || (n.kind == NodeKind::UserInput && n.id.contains("schedule"))).map(|n| n.id.clone()).collect();
    }
    if w(&["notification", "notify"]) {
        return of_kind(def, NodeKind::Notification).iter().map(|n| n.id.clone()).collect();
    }
    if w(&["login", "signin", "sign-in"]) || (w(&["sign"]) && w(&["in"])) {
        return def.nodes.iter().filter(|n| n.id.starts_with("open_") || n.id.starts_with("logged_in_") || n.id.starts_with("ask_login_")).map(|n| n.id.clone()).collect();
    }
    // Otherwise: every word of the phrase must appear in the step's name or id.
    let want: Vec<&String> = phrase_words.iter().filter(|x| !STOP.contains(&x.as_str()) && !VERBS.contains(&x.as_str())).collect();
    if want.is_empty() {
        return vec![];
    }
    def.nodes
        .iter()
        .filter(|n| {
            let hay = format!("{} {}", label(n).to_ascii_lowercase(), n.id.replace('_', " "));
            want.iter().all(|x| hay.contains(x.as_str()))
        })
        .map(|n| n.id.clone())
        .collect()
}

fn names(def: &FlowDef, ids: &[String]) -> String {
    ids.iter().filter_map(|i| def.node(i)).map(|n| format!("\"{}\"", label(n))).collect::<Vec<_>>().join(", ")
}

// ---- the rules -----------------------------------------------------------------------------------------

/// Point every step that mentions `from` at `to`.
fn retarget(def: &FlowDef, from: &'static Platform, to: &'static Platform) -> Out {
    let mut o = Out::default();
    let mut changed = 0;
    for n in &def.nodes {
        let mut cfg = Map::new();
        let mut name: Option<String> = None;
        let mut credential: Option<String> = None;
        let cfgv = n.config.as_object().cloned().unwrap_or_default();
        match n.kind {
            NodeKind::SocialMedia if cfgv.get("platform").and_then(|v| v.as_str()) == Some(from.id) => {
                cfg.insert("platform".into(), json!(to.id));
                credential = Some(platform::credential(to));
            }
            NodeKind::Website if cfgv.get("url").and_then(|v| v.as_str()).and_then(platform::from_url).is_some_and(|p| p.id == from.id) => {
                cfg.insert("url".into(), json!(to.url));
                cfg.insert("session".into(), json!(format!("{}-main", to.id)));
            }
            _ => {}
        }
        for key in ["prompt", "question", "message", "title", "preview"] {
            if let Some(s) = cfgv.get(key).and_then(|v| v.as_str()) {
                if mentions(s, from) {
                    cfg.insert(key.into(), json!(replace_platform_words(s, from, to)));
                }
            }
        }
        if mentions(&n.name, from) {
            name = Some(replace_platform_words(&n.name, from, to));
        }
        if n.credential.as_deref() == Some(&platform::credential(from)) {
            credential = Some(platform::credential(to));
        }
        if !cfg.is_empty() || name.is_some() || credential.is_some() {
            changed += 1;
            o.commands.push(Command::UpdateNode { id: n.id.clone(), name, config: if cfg.is_empty() { None } else { Some(cfg) }, enabled: None, credential });
        }
    }
    // The image size that suits the platform, when it is the only one.
    if flow_platforms(def).len() == 1 {
        for n in of_kind(def, NodeKind::ImageGeneration) {
            o.commands.push(Command::UpdateNode { id: n.id.clone(), name: None, config: Some([("size".to_string(), json!(to.image_size))].into_iter().collect()), enabled: None, credential: None });
        }
    }
    o.explain.push(format!("Switched from {} to {}: {changed} step{} updated (the website, the account, the wording). Everything else is unchanged.", from.label, to.label, if changed == 1 { "" } else { "s" }));
    if to.video_only {
        o.warnings.push(format!("{} posts are videos. Aqua cannot make videos yet, so publishing there will not work with an image.", to.label));
    }
    if to.image_required && !def.nodes.iter().any(|n| n.kind == NodeKind::ImageGeneration) {
        o.warnings.push(format!("{} needs an image on every post, and this flow has no image step. Ask me to add one.", to.label));
    }
    o.warnings.push(format!("{} will use the account saved as \"{}\".", to.label, platform::credential(to)));
    if let Some(why) = platform::not_built_note(to) {
        o.warnings.push(format!("{}: {why}. The flow is changed so you can see it and try it as a dry run, but a real run is refused before it starts.", to.label));
    }
    o
}

fn rule_swap(def: &FlowDef, c: &str, joined: &str, w: &[String]) -> Option<Out> {
    let plats = platform::mentioned(c);
    let swapish = has(w, &["instead", "switch", "change", "replace", "swap", "use", "move"]);
    if !swapish || plats.is_empty() {
        return None;
    }
    let present = flow_platforms(def);
    let (from, to): (Option<&'static Platform>, &'static Platform) = if plats.len() >= 2 {
        let (p1, p2) = (plats[0], plats[1]);
        match c.to_ascii_lowercase().find("instead of") {
            Some(io) if p1.0 < io => (Some(p2.1), p1.1),
            _ => (Some(p1.1), p2.1),
        }
    } else if joined.contains("instead") {
        (if present.len() == 1 { Some(present[0]) } else { None }, plats[0].1)
    } else {
        return None;
    };
    let Some(from) = from else {
        return Some(Out { questions: vec![Question { id: "replace".into(), question: format!("Which platform should {} replace?", to.label), choices: present.iter().map(|p| p.label.to_string()).collect(), blocking: true }], ..Default::default() });
    };
    if from.id == to.id {
        return Some(Out::say(format!("It already publishes to {}.", to.label)));
    }
    if !present.iter().any(|p| p.id == from.id) {
        return Some(Out { warnings: vec![format!("This flow does not publish to {}, so there is nothing to switch.", from.label)], ..Default::default() });
    }
    if present.iter().any(|p| p.id == to.id) {
        // The target is already there: switching is just removing the old one.
        let mut o = remove_platform(def, from);
        o.explain.insert(0, format!("It already publishes to {}, so I only removed {}.", to.label, from.label));
        return Some(o);
    }
    Some(retarget(def, from, to))
}

fn platform_step_ids(def: &FlowDef, p: &Platform) -> Vec<String> {
    def.nodes
        .iter()
        .filter(|n| {
            let cfg = &n.config;
            (n.kind == NodeKind::SocialMedia && cfg.get("platform").and_then(|v| v.as_str()) == Some(p.id))
                || (n.kind == NodeKind::Website && cfg.get("url").and_then(|v| v.as_str()).and_then(platform::from_url).is_some_and(|x| x.id == p.id))
                || (matches!(n.kind, NodeKind::Condition | NodeKind::UserInput) && (n.id.ends_with(&format!("_{}", p.id))) && (n.id.starts_with("logged_in_") || n.id.starts_with("ask_login_")))
        })
        .map(|n| n.id.clone())
        .collect()
}

fn remove_platform(def: &FlowDef, p: &'static Platform) -> Out {
    let ids = platform_step_ids(def, p);
    let mut o = Out::default();
    for id in &ids {
        o.commands.push(Command::RemoveNode { id: id.clone(), bridge: true });
    }
    o.explain.push(format!("Removed {}: {}.", p.label, names(def, &ids)));
    let left = flow_platforms(def).into_iter().filter(|x| x.id != p.id).count();
    if left == 0 {
        o.warnings.push("The flow no longer publishes anywhere.".into());
    }
    o
}

fn rule_remove_platform(def: &FlowDef, c: &str, joined: &str, w: &[String]) -> Option<Out> {
    let plats = platform::mentioned(c);
    if plats.len() != 1 || !has(w, &["remove", "delete", "drop", "stop", "skip", "without"]) || joined.contains("instead") {
        return None;
    }
    let p = plats[0].1;
    if platform_step_ids(def, p).is_empty() {
        return Some(Out { warnings: vec![format!("This flow does not publish to {}.", p.label)], ..Default::default() });
    }
    Some(remove_platform(def, p))
}

fn rule_add_platform(def: &FlowDef, c: &str, _joined: &str, w: &[String]) -> Option<Out> {
    let plats = platform::mentioned(c);
    let another = w.iter().any(|x| x == "another") && has(w, &["platform", "channel", "network", "site"]);
    if !(has(w, &["add", "also", "include", "too", "another"]) && (!plats.is_empty() || another)) || has(w, &["approval", "research", "image", "excel", "schedule"]) {
        return None;
    }
    if plats.is_empty() {
        return Some(Out { questions: vec![Question { id: "platform".into(), question: "Which platform should I add?".into(), choices: platform::PLATFORMS.iter().filter(|p| !flow_platforms(def).iter().any(|x| x.id == p.id)).map(|p| p.label.to_string()).collect(), blocking: true }], ..Default::default() });
    }
    let present = flow_platforms(def);
    let Some(last_publish) = def.nodes.iter().filter(|n| n.kind == NodeKind::SocialMedia).last().map(|n| n.id.clone()) else {
        return Some(Out { warnings: vec!["This flow has no publishing step to copy, so I cannot add another platform. Ask me to create the flow first.".into()], ..Default::default() });
    };
    let gate = of_kind(def, NodeKind::Approval).first().map(|n| n.id.clone()).or_else(|| def.nodes.iter().find(|n| n.kind == NodeKind::SocialMedia).map(|n| n.id.clone())).expect("a publishing step exists");
    let mut o = Out::default();
    let mut work = def.clone();
    for (_, p) in plats {
        if present.iter().any(|x| x.id == p.id) {
            o.explain.push(format!("It already publishes to {}.", p.label));
            continue;
        }
        let (open, check, ask, publish) = (unique_id(&work, &format!("open_{}", p.id)), unique_id(&work, &format!("logged_in_{}", p.id)), unique_id(&work, &format!("ask_login_{}", p.id)), unique_id(&work, &format!("publish_{}", p.id)));
        let image = work.nodes.iter().find(|n| n.kind == NodeKind::ImageGeneration).map(|n| n.id.clone());
        let mut cfg = json!({ "platform": p.id, "action": "publish", "text": text_ref(&work), "mode": "auto" });
        if let Some(i) = &image {
            cfg["media"] = json!(format!("{{{{nodes.{i}.image}}}}"));
        }
        let mut pub_node = NewNode::new(&publish, NodeKind::SocialMedia, &format!("Publish to {}", p.label), cfg);
        pub_node.credential = Some(platform::credential(p));
        pub_node.retry = Some(RetryPolicy { retries: 1, delay_secs: 30, backoff: Backoff::Exponential });
        pub_node.on_error = Some(OnError::AskUser);
        let cmds = vec![
            Command::AddNode { node: NewNode::new(&open, NodeKind::Website, &format!("Open {}", p.label), json!({ "url": p.url, "browser": "managed", "session": format!("{}-main", p.id) })), after: None, before: Some(gate.clone()) },
            Command::AddNode { node: NewNode::new(&check, NodeKind::Condition, &format!("Signed in to {}?", p.label), json!({ "expression": format!("{{{{nodes.{open}.logged_in}}}}") })), after: Some(Anchor { node: open.clone(), port: "out".into() }), before: None },
            Command::AddNode { node: NewNode::new(&ask, NodeKind::UserInput, &format!("Ask you to sign in to {}", p.label), json!({ "question": format!("Please sign in to {} in the Aqua browser window, then answer yes.", p.label), "answer_kind": "yes_no" })), after: None, before: None },
            Command::Connect { from: check.clone(), port: "false".into(), to: ask.clone() },
            Command::Connect { from: ask.clone(), port: "out".into(), to: gate.clone() },
            Command::AddNode { node: pub_node, after: Some(Anchor { node: last_publish.clone(), port: "out".into() }), before: None },
        ];
        if let Ok(applied) = apply(&work, &cmds) {
            work = applied.def;
        }
        o.commands.extend(cmds);
        o.explain.push(format!("Added {}: it opens the site, checks you are signed in, and publishes there after the same approval.", p.label));
        o.warnings.push(format!("{} will use the account saved as \"{}\".", p.label, platform::credential(p)));
        if let Some(why) = platform::not_built_note(p) {
            o.warnings.push(format!("{}: {why}. It is added so you can see it and try it as a dry run, but a real run of the flow is refused before it starts.", p.label));
        }
        if p.video_only {
            o.warnings.push(format!("{} posts are videos. Aqua cannot make videos yet.", p.label));
        }
        if p.image_required && image.is_none() {
            o.warnings.push(format!("{} needs an image on every post; this flow has no image step.", p.label));
        }
    }
    Some(o)
}

fn rule_no_approval(def: &FlowDef, joined: &str) -> Option<Out> {
    if !has_phrase(joined, &["dont ask", "don-t ask", "do not ask", "without asking", "no approval", "never ask", "without approval", "skip approval", "no need to ask"]) {
        return None;
    }
    let mut o = Out::default();
    o.commands.push(Command::SetApproval { policy: ApprovalPolicy::Never });
    for n in of_kind(def, NodeKind::Approval) {
        o.commands.push(Command::RemoveNode { id: n.id.clone(), bridge: true });
    }
    o.explain.push("Turned approvals off for this flow and removed the approval step.".into());
    o.warnings.push("Publishing now happens without asking you first. You can turn this back on by asking for an approval step.".into());
    Some(o)
}

fn rule_research(def: &FlowDef, w: &[String], joined: &str) -> Option<Out> {
    if !has(w, &["research", "researching"]) || has(w, &["remove", "delete", "drop", "skip", "without", "disable"]) || joined.contains("no research") {
        return None;
    }
    if !of_kind(def, NodeKind::Research).is_empty() {
        return Some(Out::say("It already researches the topic first."));
    }
    let Some(wr) = writer(def) else {
        return Some(Out { warnings: vec!["There is no writing step to put research in front of.".into()], ..Default::default() });
    };
    let id = unique_id(def, "research");
    let mut o = Out::default();
    let topic_ref = if def.variables.iter().any(|v| v.name == "topic") { "{{vars.topic}}".to_string() } else { def.name.clone() };
    o.commands.push(Command::AddNode { node: NewNode::new(&id, NodeKind::Research, "Research the topic", json!({ "topic": topic_ref, "depth": "standard", "max_sources": 5 })), after: None, before: Some(wr.id.clone()) });
    if let Some(prompt) = wr.config.get("prompt").and_then(|p| p.as_str()) {
        if !prompt.contains(&format!("{{{{nodes.{id}.")) {
            let new = format!("{} Use only facts from this research: {{{{nodes.{id}.summary}}}}", prompt.trim_end());
            o.commands.push(Command::UpdateNode { id: wr.id.clone(), name: None, config: Some([("prompt".to_string(), json!(new))].into_iter().collect()), enabled: None, credential: None });
        }
    }
    o.explain.push(format!("Added a research step in front of \"{}\", and told the writing step to use only what it finds.", label(wr)));
    Some(o)
}

fn rule_approval(def: &FlowDef, w: &[String]) -> Option<Out> {
    let wants = has(w, &["approval", "approve", "confirm", "confirmation"]) || (has(w, &["ask"]) && has(w, &["me", "first"])) || has(w, &["review"]);
    if !wants || !has(w, &["add", "require", "need", "before", "want", "include", "ask", "put", "insert"]) || has(w, &["remove", "delete", "drop", "skip", "without", "disable"]) {
        return None;
    }
    let existing = of_kind(def, NodeKind::Approval);
    let Some(target) = def.nodes.iter().find(|n| n.kind == NodeKind::SocialMedia).map(|n| n.id.clone()) else {
        return Some(Out { warnings: vec!["There is no publishing step to put an approval in front of.".into()], ..Default::default() });
    };
    let mut o = Out::default();
    if def.settings.approval == ApprovalPolicy::Never {
        o.commands.push(Command::SetApproval { policy: ApprovalPolicy::Always });
    }
    if !existing.is_empty() {
        o.explain.push(if def.settings.approval == ApprovalPolicy::Never { "Turned approvals back on.".to_string() } else { "It already asks for your approval before publishing.".to_string() });
        return Some(o);
    }
    let id = unique_id(def, "preview");
    o.commands.push(Command::AddNode { node: NewNode::new(&id, NodeKind::Approval, "Approve the post", json!({ "title": "Ready to publish", "preview": text_ref(def), "allow_edit": true })), after: None, before: Some(target.clone()) });
    o.explain.push(format!("Added an approval step in front of \"{}\": it shows a preview and waits for your OK.", def.node(&target).map(label).unwrap_or_default()));
    Some(o)
}

fn rule_schedule(def: &FlowDef, c: &str, w: &[String], now: NaiveDateTime) -> Option<Out> {
    if !(schedule_text::mentions_schedule(c) || has(w, &["schedule", "scheduled"])) || has(w, &["remove", "delete", "drop", "skip", "without", "disable"]) {
        return None;
    }
    let Some(spec) = schedule_text::parse(c, now) else {
        return Some(Out { questions: vec![Question { id: "when".into(), question: "When should it run? Give a date and time such as tomorrow at 10 AM, or how often, such as every Monday at 9.".into(), choices: vec![], blocking: true }], ..Default::default() });
    };
    let mut o = Out::default();
    if spec.repeat == Repeat::Once {
        let when = spec.when.clone().unwrap_or_default();
        if let Some(s) = of_kind(def, NodeKind::Schedule).first() {
            o.commands.push(Command::UpdateNode { id: s.id.clone(), name: None, config: Some([("when".to_string(), json!(when)), ("repeat".to_string(), json!("once"))].into_iter().collect()), enabled: Some(true), credential: None });
            o.explain.push(format!("Changed the schedule: it will publish {}.", spec.summary));
        } else {
            let Some(target) = def.nodes.iter().find(|n| n.kind == NodeKind::SocialMedia).map(|n| n.id.clone()) else {
                return Some(Out { warnings: vec!["There is no publishing step to schedule.".into()], ..Default::default() });
            };
            let id = unique_id(def, "schedule");
            o.commands.push(Command::AddNode { node: NewNode::new(&id, NodeKind::Schedule, "Wait until the scheduled time", json!({ "when": when, "repeat": "once" })), after: None, before: Some(target) });
            o.explain.push(format!("Added a schedule in front of publishing: it will publish {} (after you approve it).", spec.summary));
        }
    } else if let Some(s) = super::ScheduleSuggestion::from_spec(&spec, c, &def.settings.timezone, now) {
        // A repeating schedule is not a setting of the flow: it is created as a real schedule when this is applied.
        o.explain.push(format!("It will run by itself {}. Applying this creates the schedule; you can change it, or switch it off, in the Schedule tab.", s.summary));
        o.schedule = Some(s);
    }
    if def.settings.timezone.is_empty() {
        o.warnings.push("Times are in this computer's time zone. Set a time zone in the flow's settings to change that.".into());
    }
    Some(o)
}

fn rule_excel(def: &FlowDef, w: &[String]) -> Option<Out> {
    if !has(w, &["excel", "spreadsheet", "xlsx"]) || !has(w, &["save", "store", "log", "record", "export", "write", "add", "keep", "put"]) || has(w, &["remove", "delete", "drop", "skip", "without", "disable", "stop"]) {
        return None;
    }
    let excel = of_kind(def, NodeKind::Excel).first().map(|n| n.id.clone());
    let ask = def.nodes.iter().find(|n| n.kind == NodeKind::UserInput && (n.id.contains("excel") || label(n).to_ascii_lowercase().contains("excel"))).map(|n| n.id.clone());
    let cond = def.nodes.iter().find(|n| n.kind == NodeKind::Condition && (n.id.contains("excel") || def.predecessors(&n.id).iter().any(|p| Some(*p) == ask.as_deref()))).map(|n| n.id.clone());
    let mut o = Out::default();
    match (excel, ask) {
        (Some(_), Some(a)) => {
            o.commands.push(Command::RemoveNode { id: a, bridge: true });
            if let Some(c) = cond {
                o.commands.push(Command::RemoveNode { id: c, bridge: true });
            }
            o.explain.push("It now saves every post to Excel without asking each time.".into());
        }
        (Some(_), None) => o.explain.push("It already saves every post to Excel.".into()),
        (None, _) => {
            let Some(publish) = def.nodes.iter().filter(|n| n.kind == NodeKind::SocialMedia).last().map(|n| n.id.clone()) else {
                return Some(Out { warnings: vec!["There is no post to save yet.".into()], ..Default::default() });
            };
            let id = unique_id(def, "save_row");
            let labels: Vec<&str> = flow_platforms(def).iter().map(|p| p.label).collect();
            let cols = json!({ "Platform": labels.join(" and "), "Post": text_ref(def), "Status": format!("{{{{nodes.{publish}.status}}}}"), "URL": format!("{{{{nodes.{publish}.post_url}}}}"), "Date": "{{now.date}}" });
            o.commands.push(Command::AddNode { node: NewNode::new(&id, NodeKind::Excel, "Save to Excel", json!({ "operation": "append_row", "path": "posts.xlsx", "sheet": "Posts", "columns": cols })), after: Some(Anchor { node: publish, port: "out".into() }), before: None });
            o.explain.push("Added a step after publishing that saves the platform, text, status, link and date to Excel (posts.xlsx).".into());
        }
    }
    Some(o)
}

fn rule_tone(def: &FlowDef, w: &[String]) -> Option<Out> {
    let tone = ["professional", "friendly", "casual", "technical", "persuasive"].iter().find(|t| w.iter().any(|x| x == *t))?;
    if !has(w, &["make", "more", "tone", "sound", "write", "use", "change"]) || has(w, &["remove", "delete", "drop"]) {
        return None;
    }
    let mut o = Out::default();
    for n in of_kind(def, NodeKind::TextGeneration).into_iter().filter(|n| !is_image_step(n)) {
        o.commands.push(Command::UpdateNode { id: n.id.clone(), name: None, config: Some([("tone".to_string(), json!(tone))].into_iter().collect()), enabled: None, credential: None });
    }
    if o.commands.is_empty() {
        return Some(Out { warnings: vec!["There is no writing step to change the tone of.".into()], ..Default::default() });
    }
    o.explain.push(format!("Changed the tone of the writing to {tone}."));
    Some(o)
}

fn rule_toggle(def: &FlowDef, w: &[String], joined: &str) -> Option<Out> {
    let off = has(w, &["disable", "deactivate"]) || joined.contains("turn off") || joined.contains("switch off");
    let on = has(w, &["enable", "activate"]) || joined.contains("turn on") || joined.contains("switch on");
    if !(off || on) {
        return None;
    }
    let ids = things(def, w);
    if ids.is_empty() {
        return Some(Out { warnings: vec!["I could not tell which step you mean.".into()], ..Default::default() });
    }
    let mut o = Out::default();
    for id in &ids {
        o.commands.push(Command::UpdateNode { id: id.clone(), name: None, config: None, enabled: Some(on && !off), credential: None });
    }
    o.explain.push(format!("{} {}. A switched-off step is skipped and the flow carries on.", if off { "Switched off" } else { "Switched on" }, names(def, &ids)));
    Some(o)
}

fn rule_remove(def: &FlowDef, w: &[String], joined: &str) -> Option<Out> {
    if !(has(w, &["remove", "delete", "drop", "skip", "without"]) || joined.contains("get rid of") || joined.contains("no longer") || w.first().is_some_and(|x| x == "no")) {
        return None;
    }
    let ids = things(def, w);
    if ids.is_empty() {
        return Some(Out { warnings: vec!["I could not tell which step you mean. Click the step in the diagram and delete it, or tell me its name.".into()], ..Default::default() });
    }
    if ids.len() > 3 {
        return Some(Out { questions: vec![Question { id: "which_step".into(), question: format!("That matches {} steps ({}). Which do you mean?", ids.len(), names(def, &ids)), choices: ids.iter().filter_map(|i| def.node(i)).map(label).collect(), blocking: true }], ..Default::default() });
    }
    let mut o = Out::default();
    for id in &ids {
        o.commands.push(Command::RemoveNode { id: id.clone(), bridge: true });
    }
    o.explain.push(format!("Removed {}, and joined the steps around {} so the flow still connects.", names(def, &ids), if ids.len() == 1 { "it" } else { "them" }));
    if has(w, &["image", "images", "picture", "photo", "illustration", "graphic", "visual"]) {
        for p in flow_platforms(def).into_iter().filter(|p| p.image_required) {
            o.warnings.push(format!("{} needs an image on every post, so publishing there will fail without one.", p.label));
        }
    }
    Some(o)
}

fn rule_rename(c: &str, w: &[String]) -> Option<Out> {
    if !has(w, &["rename", "call"]) {
        return None;
    }
    let lower = c.to_ascii_lowercase();
    let i = lower.rfind(" to ").or_else(|| lower.rfind(" it "))?;
    let name = c[i + 4..].trim().trim_matches(|ch| ch == '"' || ch == '\'').trim().to_string();
    if name.is_empty() || name.chars().count() > 80 {
        return None;
    }
    Some(Out { commands: vec![Command::Rename { name: name.clone() }], explain: vec![format!("Renamed the flow to \"{name}\".")], ..Default::default() })
}

fn run_rules(def: &FlowDef, clause: &str, now: NaiveDateTime) -> Option<Out> {
    let w = words_lower(clause);
    let joined = w.join(" ");
    // Naming the flow comes first: a name such as "Weekly AI news" must not be read as a schedule.
    rule_rename(clause, &w)
        .or_else(|| rule_no_approval(def, &joined))
        .or_else(|| rule_toggle(def, &w, &joined))
        .or_else(|| rule_swap(def, clause, &joined, &w))
        .or_else(|| rule_remove_platform(def, clause, &joined, &w))
        .or_else(|| rule_add_platform(def, clause, &joined, &w))
        .or_else(|| rule_research(def, &w, &joined))
        .or_else(|| rule_approval(def, &w))
        .or_else(|| rule_schedule(def, clause, &w, now))
        .or_else(|| rule_excel(def, &w))
        .or_else(|| rule_tone(def, &w))
        .or_else(|| rule_remove(def, &w, &joined))
}

/// Split a request into the changes it holds. A clause with no verb of its own ("... and the
/// research") borrows the previous clause's verb.
fn clauses(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let text = text.replace('\n', ". ").replace(", then ", ". ").replace(" then ", ". ");
    for sentence in text.split(['.', ';', '!']) {
        let s = sentence.trim();
        if s.is_empty() {
            continue;
        }
        // " and " splits only when what follows has a verb or a platform of its own.
        let mut rest = s;
        loop {
            let lower = rest.to_ascii_lowercase();
            let Some(i) = lower.find(" and ") else {
                out.push(rest.trim().to_string());
                break;
            };
            let (head, tail) = (&rest[..i], &rest[i + 5..]);
            // Both halves need a few words of their own; "and" inside a short phrase stays.
            if head.split_whitespace().count() >= 2 && tail.split_whitespace().count() >= 2 {
                out.push(head.trim().to_string());
                rest = tail;
            } else {
                out.push(rest.trim().to_string());
                break;
            }
        }
    }
    let mut result = Vec::new();
    let mut last_verb: Option<String> = None;
    for c in out {
        let w = words_lower(&c);
        if let Some(v) = w.iter().find(|x| VERBS.contains(&x.as_str())) {
            last_verb = Some(v.clone());
            result.push(c);
        } else if let (Some(v), true) = (&last_verb, w.len() <= 4) {
            // "... and the research": a short phrase with no verb of its own takes the previous verb.
            result.push(format!("{v} {c}"));
        } else {
            result.push(c);
        }
    }
    result
}

pub fn plan_edit(def: &FlowDef, text: &str, answers: &Answers, now: NaiveDateTime) -> EditPlan {
    let mut plan = EditPlan::default();
    let mut state = def.clone();
    let mut text = text.to_string();
    // An answer to an earlier question is folded into the request it belongs to.
    if let Some(a) = answers.get("platform").filter(|a| !a.trim().is_empty()) {
        if platform::mentioned(&text).is_empty() {
            text = format!("{text} {a}");
        }
    }
    if let Some(a) = answers.get("when").filter(|a| !a.trim().is_empty()) {
        text = format!("schedule it {a}");
    }
    if let Some(a) = answers.get("replace").filter(|a| !a.trim().is_empty()) {
        text = format!("{text} {a}");
    }
    for clause in clauses(&text) {
        match run_rules(&state, &clause, now) {
            None => plan.not_understood.push(clause),
            Some(out) => {
                plan.explanation.extend(out.explain);
                if out.schedule.is_some() {
                    plan.schedule = out.schedule.clone();
                }
                plan.warnings.extend(out.warnings);
                plan.questions.extend(out.questions);
                if !out.commands.is_empty() {
                    match apply(&state, &out.commands) {
                        Ok(applied) => {
                            state = applied.def;
                            plan.explanation.extend(applied.notes);
                            plan.commands.extend(out.commands);
                        }
                        Err(e) => plan.warnings.push(format!("I could not make that change: {e}")),
                    }
                }
            }
        }
    }
    plan
}

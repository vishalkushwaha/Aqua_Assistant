//! Builds a new flow from an [`Intent`], as a list of commands, with a reason for every step.
//!
//! The shape is always the same skeleton (start, research, write, image, sign in, approval, schedule,
//! publish, save), with only the parts the request called for. That shape is what a person can read and
//! edit, and it is what the validator has already been shown to accept.

use super::intent::Intent;
use super::{Plan, Question};
use crate::flows::commands::{Command, NewNode};
use crate::flows::model::*;
use crate::flows::platform::{self, Platform};
use crate::flows::registry;
use crate::flows::schedule_text::Repeat;
use serde_json::{json, Value};
use std::collections::HashMap;

pub type Answers = HashMap<String, String>;

struct Builder {
    cmds: Vec<Command>,
    /// Where the next step attaches: (step, exit). More than one after a branch that rejoins.
    tails: Vec<(String, String)>,
    reasons: Vec<(String, String)>,
}

impl Builder {
    /// The first step: nothing to connect it to.
    fn add(&mut self, node: NewNode, reason: &str) {
        let id = node.id.clone();
        let port = registry::spec(node.kind).out_ports[0].clone();
        self.cmds.push(Command::AddNode { node, after: None, before: None });
        self.reasons.push((id.clone(), reason.to_string()));
        self.tails = vec![(id, port)];
    }

    /// Add a step and connect every current tail to it.
    fn chain(&mut self, node: NewNode, reason: &str) {
        let id = node.id.clone();
        let tails = std::mem::take(&mut self.tails);
        let port = registry::spec(node.kind).out_ports[0].clone();
        self.cmds.push(Command::AddNode { node, after: None, before: None });
        for (f, p) in tails {
            self.cmds.push(Command::Connect { from: f, port: p, to: id.clone() });
        }
        self.reasons.push((id.clone(), reason.to_string()));
        self.tails = vec![(id, port)];
    }

    fn connect(&mut self, from: &str, port: &str, to: &str) {
        self.cmds.push(Command::Connect { from: from.into(), port: port.into(), to: to.into() });
    }
}

fn quoted(list: &[&Platform]) -> String {
    list.iter().map(|p| p.label).collect::<Vec<_>>().join(" and ")
}

/// The whole flow for a request. `answers` fill in what an earlier plan asked about.
pub fn compose(intent: &Intent, answers: &Answers, request: &str) -> Plan {
    let mut questions: Vec<Question> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // ---- what is still missing --------------------------------------------------------------
    let mut platforms: Vec<&'static Platform> = intent.platforms.clone();
    if let Some(a) = answers.get("platform") {
        for p in platform::mentioned(a) {
            if !platforms.iter().any(|x| x.id == p.1.id) {
                platforms.push(p.1);
            }
        }
    }
    if intent.publish && platforms.is_empty() {
        questions.push(Question { id: "platform".into(), question: "Which platform should I publish to?".into(), choices: platform::PLATFORMS.iter().map(|p| p.label.to_string()).collect(), blocking: true });
        return Plan { commands: vec![], reasons: vec![], questions, warnings, title: "Which platform?".into(), schedule: None };
    }
    let topic = answers.get("topic").map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).or_else(|| intent.topic.clone());
    if topic.is_none() {
        questions.push(Question { id: "topic".into(), question: "What topic should it cover?".into(), choices: vec![], blocking: false });
    }
    let mut schedule = intent.schedule.clone();
    if let Some(a) = answers.get("when") {
        if !a.trim().is_empty() && !a.eq_ignore_ascii_case("now") {
            match crate::flows::schedule_text::parse(a, chrono::Local::now().naive_local()) {
                Some(s) => schedule = Some(s),
                None => questions.push(Question { id: "when".into(), question: format!("I could not read \"{a}\" as a date and time. When should it be published (for example tomorrow at 10 AM)?"), choices: vec![], blocking: false }),
            }
        }
    }
    if intent.schedule_unclear && schedule.is_none() && !questions.iter().any(|q| q.id == "when") {
        questions.push(Question { id: "when".into(), question: "When should it run? Please give a date and time, such as tomorrow at 10 AM, or how often, such as every Monday at 9.".into(), choices: vec![], blocking: false });
    }
    if intent.publish && schedule.is_none() && !intent.schedule_unclear && !answers.contains_key("when") {
        questions.push(Question { id: "when".into(), question: "Publish as soon as you approve it, or schedule it? (Say for example: tomorrow at 10 AM.)".into(), choices: vec!["now".into()], blocking: false });
    }
    if intent.excel_source {
        warnings.push("Posting from an Excel file is not built yet (it comes with the spreadsheet step). I built the single-post flow; the spreadsheet part will be added then.".into());
    }
    for p in &platforms {
        if p.video_only {
            warnings.push(format!("{} posts are videos. Aqua cannot make videos yet, so publishing there will not work with an image.", p.label));
        }
        if p.image_required && !intent.image {
            warnings.push(format!("{} needs an image on every post, so I added an image step.", p.label));
        }
    }
    let image = intent.image || platforms.iter().any(|p| p.image_required);
    let research = intent.research;
    let recurring = schedule.as_ref().filter(|s| s.repeat != Repeat::Once).cloned();
    let one_time = schedule.as_ref().filter(|s| s.repeat == Repeat::Once).cloned();
    let platform_label = if platforms.len() == 1 { platforms[0].label.to_string() } else { "social media".to_string() };
    let tone = intent.tone.clone().unwrap_or_else(|| "professional".into());

    let mut b = Builder { cmds: vec![], tails: vec![], reasons: vec![] };
    let title_words: String = request.split_whitespace().take(8).collect::<Vec<_>>().join(" ");
    let name = if platforms.is_empty() { format!("Flow: {title_words}") } else { format!("{} post", quoted(&platforms)) };
    b.cmds.push(Command::Rename { name: name.chars().take(80).collect() });
    b.cmds.push(Command::SetDescription { description: request.chars().take(400).collect::<String>().trim().to_string() });
    b.cmds.push(Command::AddVariable { variable: VariableDef { name: "topic".into(), label: "Topic".into(), kind: VarKind::Text, required: true, default: topic.as_ref().map(|t| json!(t)), choices: vec![] } });
    if intent.no_approval {
        b.cmds.push(Command::SetApproval { policy: ApprovalPolicy::Never });
        warnings.push("You said not to ask before publishing, so the flow publishes without asking. You can turn this back on in the flow's settings.".into());
    }

    // ---- start ---------------------------------------------------------------------------------------
    // A repeating schedule is created separately when the flow is saved (it is not a setting of the start step).
    let trigger_cfg = json!({ "trigger_type": "manual" });
    let suggestion = recurring.as_ref().and_then(|s| super::ScheduleSuggestion::from_spec(s, request, "", chrono::Local::now().naive_local()));
    let start_reason = match &suggestion {
        Some(s) => format!("You asked for it to run {}: saving it creates that schedule (you can change it in the Schedule tab). The flow itself starts when it is run.", s.summary),
        None => "Every flow starts with a trigger; this one starts when you press Run.".to_string(),
    };
    b.add(NewNode::new("start", NodeKind::Trigger, "Start", trigger_cfg), &start_reason);

    // ---- research ---------------------------------------------------------------------------------------
    if research {
        b.chain(NewNode::new("research", NodeKind::Research, "Research the topic", json!({ "topic": "{{vars.topic}}", "depth": "standard", "max_sources": 5 })), "You asked for current information, so it researches first and keeps the sources.");
    }

    // ---- write ---------------------------------------------------------------------------------------------
    let facts = if research { " Use only facts from this research: {{nodes.research.summary}}" } else { "" };
    let (write_name, write_prompt) = if intent.publish || !platforms.is_empty() {
        ("Write the post", format!("Write an engaging {platform_label} post about {{{{vars.topic}}}}.{facts} End with three to five relevant hashtags."))
    } else {
        ("Write the summary", format!("Write a clear, short summary about {{{{vars.topic}}}}.{facts}"))
    };
    b.chain(NewNode::new("write_post", NodeKind::TextGeneration, write_name, json!({ "prompt": write_prompt, "tone": tone, "max_words": 200 })), "The text is written by the language model from your topic and the research, in the tone you chose.");

    // ---- image ------------------------------------------------------------------------------------------------
    if image {
        b.chain(NewNode::new("image_prompt", NodeKind::TextGeneration, "Write the image prompt", json!({ "prompt": "Write one short prompt for a professional image to go with this post: {{nodes.write_post.text}}", "max_words": 40 })), "An image is made from a prompt, so the model first turns the post into one.");
        let size = if platforms.len() == 1 { platforms[0].image_size } else { "1024x1024" };
        b.chain(NewNode::new("make_image", NodeKind::ImageGeneration, "Generate the image", json!({ "prompt": "{{nodes.image_prompt.text}}", "provider": "diffusion", "size": size })), "You asked for an image. It is saved with the run so later steps can attach it.");
    }

    // ---- sign in to each platform -----------------------------------------------------------------------------
    for p in &platforms {
        let (open, check, ask) = (format!("open_{}", p.id), format!("logged_in_{}", p.id), format!("ask_login_{}", p.id));
        b.chain(NewNode::new(&open, NodeKind::Website, &format!("Open {}", p.label), json!({ "url": p.url, "browser": "managed", "session": format!("{}-main", p.id) })), &format!("It opens {} in Aqua's browser and checks whether you are signed in.", p.label));
        b.chain(NewNode::new(&check, NodeKind::Condition, &format!("Signed in to {}?", p.label), json!({ "expression": format!("{{{{nodes.{open}.logged_in}}}}") })), "If you are not signed in, it asks you to sign in instead of guessing.");
        let tails = vec![(check.clone(), "true".to_string())];
        b.tails = vec![];
        b.cmds.push(Command::AddNode { node: NewNode::new(&ask, NodeKind::UserInput, &format!("Ask you to sign in to {}", p.label), json!({ "question": format!("Please sign in to {} in the Aqua browser window, then answer yes.", p.label), "answer_kind": "yes_no" })), after: None, before: None });
        b.connect(&check, "false", &ask);
        b.reasons.push((ask.clone(), format!("Aqua never asks for your {} password; you sign in yourself.", p.label)));
        b.tails = tails;
        b.tails.push((ask, "out".into()));
    }

    // ---- approval ------------------------------------------------------------------------------------------------
    let has_publish = !platforms.is_empty();
    if has_publish && !intent.no_approval {
        b.chain(NewNode::new("preview", NodeKind::Approval, "Approve the post", json!({ "title": "Ready to publish", "preview": "{{nodes.write_post.text}}", "allow_edit": true })), "Publishing cannot be undone, so it shows you a preview and waits for your OK.");
    }

    // ---- schedule (once) ---------------------------------------------------------------------------------------------
    if let (Some(s), true) = (&one_time, has_publish) {
        b.chain(NewNode::new("schedule", NodeKind::Schedule, "Wait until the scheduled time", json!({ "when": s.when, "repeat": "once" })), &format!("You asked for it {}; the flow waits until then after you approve.", s.summary));
    }

    // ---- publish ----------------------------------------------------------------------------------------------------------
    for p in &platforms {
        let mut cfg = json!({ "platform": p.id, "action": "publish", "text": "{{nodes.write_post.text}}", "mode": "auto" });
        if image {
            cfg["media"] = Value::String("{{nodes.make_image.image}}".into());
        }
        let mut n = NewNode::new(&format!("publish_{}", p.id), NodeKind::SocialMedia, &format!("Publish to {}", p.label), cfg);
        n.credential = Some(platform::credential(p));
        n.retry = Some(RetryPolicy { retries: 1, delay_secs: 30, backoff: Backoff::Exponential });
        n.on_error = Some(OnError::AskUser);
        b.chain(n, &format!("Posts to {} through its connector if one is set up, otherwise through the browser, using the account saved as {}.", p.label, platform::credential(p)));
        match platform::not_built_note(p) {
            Some(why) => warnings.push(format!("{}: {why}. I built the flow so you can see it and try it as a dry run, but a real run is refused before it starts. Ask me to switch it to LinkedIn to get one that posts.", p.label)),
            None => warnings.push(format!("{}: uses the account saved as \"{}\". No connector is set up yet, so it will use the browser.", p.label, platform::credential(p))),
        }
    }
    if !has_publish {
        b.chain(NewNode::new("show_result", NodeKind::Notification, "Show me the result", json!({ "channel": "in_app", "message": "{{nodes.write_post.text}}" })), "You did not ask to publish anywhere, so it shows you the result.");
    }

    // ---- save to Excel, and what next ----------------------------------------------------------------------------------------
    let last_publish = platforms.last().map(|p| format!("publish_{}", p.id));
    let columns = |status: &str, url: &str| {
        json!({ "Platform": quoted(&platforms), "Post": "{{nodes.write_post.text}}", "Status": status, "URL": url, "Date": "{{now.date}}" })
    };
    if intent.excel {
        let (status, url) = match &last_publish {
            Some(id) => (format!("{{{{nodes.{id}.status}}}}"), format!("{{{{nodes.{id}.post_url}}}}")),
            None => ("drafted".to_string(), String::new()),
        };
        b.chain(NewNode::new("save_row", NodeKind::Excel, "Save to Excel", json!({ "operation": "append_row", "path": "posts.xlsx", "sheet": "Posts", "columns": columns(&status, &url) })), "You asked for the post to be saved to Excel.");
    } else if let Some(id) = &last_publish {
        b.chain(NewNode::new("ask_excel", NodeKind::UserInput, "Save to Excel?", json!({ "question": "Save this post to Excel?", "answer_kind": "yes_no" })), "After publishing, it asks whether to keep a record in Excel.");
        b.chain(NewNode::new("want_excel", NodeKind::Condition, "Save it?", json!({ "expression": "{{nodes.ask_excel.answer}}" })), "Only saves if you say yes.");
        let excel = NewNode::new("save_row", NodeKind::Excel, "Save to Excel", json!({ "operation": "append_row", "path": "posts.xlsx", "sheet": "Posts", "columns": columns(&format!("{{{{nodes.{id}.status}}}}"), &format!("{{{{nodes.{id}.post_url}}}}")) }));
        b.cmds.push(Command::AddNode { node: excel, after: None, before: None });
        b.connect("want_excel", "true", "save_row");
        b.reasons.push(("save_row".into(), "Adds a row: platform, text, status, link and date.".into()));
        b.tails = vec![("want_excel".into(), "false".into()), ("save_row".into(), "out".into())];
    }
    if has_publish && one_time.is_none() && recurring.is_none() {
        b.chain(NewNode::new("ask_schedule", NodeKind::UserInput, "Schedule another?", json!({ "question": "Schedule another post?", "answer_kind": "yes_no" })), "It asks whether you want another post scheduled.");
    }
    b.cmds.push(Command::Layout);
    Plan { commands: b.cmds, reasons: b.reasons, questions, warnings, title: name, schedule: suggestion }
}

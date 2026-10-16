//! The AI builder: turns a sentence into a flow, or into a change to an existing flow.
//!
//! Planning and applying are separate. `plan` only *proposes*: a list of commands, what they would
//! change (a diff), why each step is there, what still needs asking, and whether the result would
//! pass validation. Nothing is stored until the person accepts, and accepting applies the same
//! commands again on the server, against the flow as it is then. The order is the one in the spec:
//! planner, structured commands, validator, permission check, and only then the engine.

pub mod compose;
pub mod edit;
pub mod intent;
pub mod llm;
pub mod resolver;

use super::commands::{apply, Command};
use super::diff::{diff, Diff};
use super::model::*;
use super::validate::{validate, Report};
use chrono::NaiveDateTime;
pub use compose::Answers;
pub use llm::{install, installed, Completer};
pub use resolver::Resolution;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub question: String,
    #[serde(default)]
    pub choices: Vec<String>,
    /// A blocking question must be answered before there is anything to apply.
    pub blocking: bool,
}

/// What the composer and the rules hand to `plan`.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub commands: Vec<Command>,
    /// (step id, why it is there)
    pub reasons: Vec<(String, String)>,
    pub questions: Vec<Question>,
    pub warnings: Vec<String>,
    pub title: String,
    pub schedule: Option<ScheduleSuggestion>,
}

/// A schedule the person asked for in words. It is not part of the flow: applying the proposal creates it as a real
/// schedule (see `sched`), which they can change or switch off in the Schedule tab.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct ScheduleSuggestion {
    pub cron: String,
    /// In words: "every day at 10:00, for 4 weeks (until 2030-01-29)".
    pub summary: String,
    /// The flow's own time zone setting; empty means this computer's.
    #[serde(default)]
    pub timezone: String,
    #[serde(default)]
    pub end_on: Option<String>,
    #[serde(default)]
    pub max_runs: Option<u32>,
}

impl ScheduleSuggestion {
    pub fn from_spec(spec: &crate::flows::schedule_text::ScheduleSpec, request: &str, timezone: &str, now: NaiveDateTime) -> Option<Self> {
        let cron = spec.cron.clone()?;
        let lim = crate::flows::schedule_text::limits(request, now);
        let summary = if lim.summary.is_empty() { spec.summary.clone() } else { format!("{}, {}", spec.summary, lim.summary) };
        Some(Self { cron, summary, timezone: timezone.to_string(), end_on: lim.end_on, max_runs: lim.max_runs })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Proposal {
    /// "create" or "edit"
    pub kind: &'static str,
    /// False when nothing could be made of the request.
    pub understood: bool,
    pub title: String,
    /// Sentences for the person: what changes and why.
    pub explanation: Vec<String>,
    pub questions: Vec<Question>,
    pub warnings: Vec<String>,
    pub commands: Vec<Command>,
    pub base_version: Option<i64>,
    /// The flow as it would be after the commands.
    pub result: FlowDef,
    pub diff: Diff,
    pub validation: Report,
    pub resolution: Vec<Resolution>,
    /// "rules", "model" or "rules+model"
    pub source: &'static str,
    /// What was not understood, by clause.
    pub not_understood: Vec<String>,
    /// A repeating schedule the request asked for, created when the proposal is applied.
    pub schedule: Option<ScheduleSuggestion>,
}

const CAN_DO: &str = "I can: switch the platform (\"switch LinkedIn to Instagram\"), add or remove a platform, add or remove research, the image or the approval step, schedule it (\"tomorrow at 10 AM\", \"every Monday at 9\"), save to Excel, change the tone, switch a step off, or rename the flow. For anything else, click a step and edit it.";

fn errors_of(r: &Report) -> Vec<(String, Option<String>)> {
    r.errors().map(|i| (i.code.clone(), i.node.clone())).collect()
}

/// Read a request and propose. `base` is the flow being edited, or `None` to create a new one.
pub fn plan(request: &str, base: Option<&Flow>, answers: &Answers, now: NaiveDateTime, model: Option<&dyn Completer>, history: &[(String, String)]) -> Proposal {
    let request = request.trim();
    let blank = FlowDef::default();
    let base_def = base.map(|f| &f.def).unwrap_or(&blank);
    let mut explanation: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut questions: Vec<Question> = Vec::new();
    let mut commands: Vec<Command> = Vec::new();
    let mut not_understood: Vec<String> = Vec::new();
    let mut schedule: Option<ScheduleSuggestion> = None;
    let mut source = "rules";
    let mut title = String::new();

    if base.is_none() {
        let intent = intent::parse_create(request, now);
        let recognised = intent.publish || !intent.platforms.is_empty() || intent.research || intent.image || intent.excel || intent.schedule.is_some();
        if recognised {
            let p = compose::compose(&intent, answers, request);
            for (id, why) in &p.reasons {
                let name = p.commands.iter().find_map(|c| match c {
                    Command::AddNode { node, .. } if node.id == *id => Some(node.name.clone()),
                    _ => None,
                });
                explanation.push(format!("{}: {why}", name.unwrap_or_else(|| id.clone())));
            }
            commands = p.commands;
            questions = p.questions;
            warnings = p.warnings;
            title = p.title;
            schedule = p.schedule;
        } else if let Some(m) = model {
            let scaffold = FlowDef { name: request.chars().take(60).collect(), nodes: vec![Node::new("start", NodeKind::Trigger, "Start").with_config(serde_json::json!({ "trigger_type": "manual" }))], ..Default::default() };
            match llm::plan_with_model(m, &scaffold, request, history) {
                Ok(mp) => {
                    commands = vec![Command::Rename { name: scaffold.name.clone() }, Command::AddNode { node: super::commands::NewNode::new("start", NodeKind::Trigger, "Start", serde_json::json!({ "trigger_type": "manual" })), after: None, before: None }];
                    commands.extend(mp.commands);
                    commands.push(Command::Layout);
                    if !mp.explanation.is_empty() {
                        explanation.push(mp.explanation);
                    }
                    if let Some(q) = mp.question {
                        questions.push(q);
                    }
                    title = scaffold.name;
                    source = "model";
                }
                Err(e) => warnings.push(format!("The language model could not build this either ({e}).")),
            }
        }
    } else {
        let ep = edit::plan_edit(base_def, request, answers, now);
        explanation = ep.explanation;
        warnings = ep.warnings;
        questions = ep.questions;
        commands = ep.commands;
        not_understood = ep.not_understood;
        schedule = ep.schedule;
        if !not_understood.is_empty() {
            if let Some(m) = model {
                // What the rules did is applied first; the model is shown the flow as it would then be.
                let mid = apply(base_def, &commands).map(|a| a.def).unwrap_or_else(|_| base_def.clone());
                let rules_did_something = !commands.is_empty() || !explanation.is_empty();
                let rest = not_understood.join(". ");
                match llm::plan_with_model(m, &mid, &rest, history) {
                    Ok(mp) => {
                        commands.extend(mp.commands);
                        if !mp.explanation.is_empty() {
                            explanation.push(mp.explanation);
                        }
                        if let Some(q) = mp.question {
                            questions.push(q);
                        }
                        not_understood.clear();
                        source = if rules_did_something { "rules+model" } else { "model" };
                    }
                    Err(e) => warnings.push(format!("I did not understand \"{rest}\", and the language model could not help either ({e}).")),
                }
            }
        }
        title = explanation.first().map(|s| s.chars().take(80).collect()).unwrap_or_default();
    }

    let applied = apply(base_def, &commands);
    let (result, notes) = match applied {
        Ok(a) => (a.def, a.notes),
        Err(e) => {
            warnings.push(format!("The proposed change could not be applied: {e}"));
            commands.clear();
            (base_def.clone(), vec![])
        }
    };
    explanation.extend(notes);
    let understood = !commands.is_empty() || !questions.is_empty() || (base.is_some() && !explanation.is_empty());
    if !understood && not_understood.is_empty() && !request.is_empty() {
        not_understood.push(request.to_string());
    }
    if !understood {
        explanation.push(format!("I could not turn that into a change. {CAN_DO}"));
    }
    // A change must not leave the flow worse than it was.
    let validation = validate(&result);
    if base.is_some() {
        let before = errors_of(&validate(base_def));
        let introduced: Vec<String> = errors_of(&validation).into_iter().filter(|e| !before.contains(e)).map(|(c, n)| format!("{c}{}", n.map(|n| format!(" at {n}")).unwrap_or_default())).collect();
        if !introduced.is_empty() {
            warnings.push(format!("This change would leave the flow with problems ({}). It is shown so you can see them; fix them before running.", introduced.join(", ")));
        }
    }
    let d = diff(base_def, &result);
    let resolution = resolver::resolve(&result);
    if title.is_empty() {
        title = if base.is_some() { "Change the flow".into() } else { result.name.clone() };
    }
    Proposal { kind: if base.is_some() { "edit" } else { "create" }, understood, title, explanation, questions, warnings, commands, base_version: base.map(|f| f.version), result, diff: d, validation, resolution, source, not_understood, schedule }
}

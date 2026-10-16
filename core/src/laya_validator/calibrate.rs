//! A change to a threshold of the decision router, only through a test (card R7-13). This module proposes; it never applies.
//!
//! The router moves a request between chat and tools only when the model is as sure as a threshold says (`MOVE_PROBABILITY`, the margin and the share of the letters). Each answer of
//! a person ("Was Chat", "Needed Tools") is a labelled example, and each example kept with the model's answer can be asked again: what would the router have done with another
//! threshold? [`evaluate`] tries each candidate on (a) those examples and (b) the held-out requests, and offers a candidate only if
//!
//! * there are enough of both with a recorded model answer ([`MIN_FEEDBACK`], [`MIN_HELD_OUT`]): fewer say nothing;
//! * the held-out requests are not routed worse (not one more wrong, whatever the gain elsewhere); and
//! * the answers of the person are routed right in at least [`FEEDBACK_MARGIN_POINTS`] more of every hundred.
//!
//! Anything else is no proposal, with the reason. The proposal is data (what to change, from what to what, and the numbers); nothing in this module writes a setting, and the
//! thresholds of the router are constants in `decision_route.rs` that only a person's edit changes.
//!
//! The held-out requests are `core/tests/fixtures/routing_held_out.json`: the labelled requests of the gate's four fixture sets, with the model's answer for each where a live
//! measurement recorded one. Without that measurement there is no held-out set to hold a change against, and no proposal is made.

use super::decision_route::{combine, thresholds};
use super::feedback_stats::{latest_answers, Evidence, FeedbackRow, ModelAnswer};
use super::{GateDecision, GateRoute, LayaGate, SurfaceHint};
use crate::modules::decision::Thresholds;
use serde::{Deserialize, Serialize};

/// The fewest answers of the person, each kept with the model's answer, that a proposal may rest on.
pub const MIN_FEEDBACK: usize = 20;
/// The fewest held-out requests with a recorded model answer that a change is held against.
pub const MIN_HELD_OUT: usize = 30;
/// How many more of every hundred answers must be routed right with the new threshold: with 20 answers, one more is enough; with 40, two.
pub const FEEDBACK_MARGIN_POINTS: usize = 5;

/// The values tried for the share of the letters the model must have for its choice (the router uses 0.98).
const PROBABILITY_GRID: [f64; 11] = [0.80, 0.85, 0.90, 0.93, 0.95, 0.96, 0.97, 0.98, 0.985, 0.99, 0.995];
/// The values tried for the lead over the next answer (the router uses 0.9).
const MARGIN_GRID: [f64; 6] = [0.50, 0.60, 0.70, 0.80, 0.90, 0.95];

const FIXTURE: &str = include_str!("../../tests/fixtures/routing_held_out.json");

/// What the router would have decided, asked again with other thresholds: the rules' decision stands unless the model was asked and is sure enough. The router is taken as on and
/// allowed to refuse, which is the whole of the policy that the thresholds belong to.
pub fn replay(evidence: &Evidence, thresholds: &Thresholds) -> GateRoute {
    let Some(model) = evidence.model else { return evidence.rules_route };
    let rules = GateDecision {
        route: evidence.rules_route,
        needs_tools: evidence.rules_route == GateRoute::McpAgent,
        jailbreak: 0.0,
        prompt_injection: 0.0,
        harm_severity: 0.0,
        confidence: evidence.rules_confidence,
        surface: SurfaceHint::Either,
        source: "rules".into(),
        latency_ms: 0,
    };
    combine(&rules, Some(&model.as_decided()), thresholds, evidence.mcp_available, true).route
}

/// A request with a label that was not used to tune the rules or the thresholds, and what the router had for it.
#[derive(Debug, Clone, PartialEq)]
pub struct HeldOut {
    pub id: String,
    pub expect: GateRoute,
    pub evidence: Evidence,
}

/// A change the held-out requests and the person's answers support.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Proposal {
    /// Which threshold: `min_probability` or `min_margin`.
    pub name: String,
    pub from: f64,
    pub to: f64,
    /// The share of held-out requests routed right with the threshold as it is, and with the change.
    pub held_out_before: f64,
    pub held_out_after: f64,
    pub n_held_out: usize,
    /// The same for the person's answers.
    pub feedback_before: f64,
    pub feedback_after: f64,
    pub n_feedback: usize,
}

/// Why there is no proposal, as a code a program can read and a sentence a person can.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NoProposal {
    pub code: &'static str,
    pub reason: String,
}

impl NoProposal {
    fn new(code: &'static str, reason: String) -> Self {
        Self { code, reason }
    }
}

struct Example {
    evidence: Evidence,
    expect: GateRoute,
}

fn right(examples: &[Example], thresholds: &Thresholds) -> usize {
    examples.iter().filter(|e| replay(&e.evidence, thresholds) == e.expect).count()
}

/// Every threshold tried, one at a time: its name, the value it has, the value tried, and the thresholds with that one changed.
fn candidates(base: &Thresholds) -> Vec<(&'static str, f64, f64, Thresholds)> {
    let mut out = Vec::new();
    for to in PROBABILITY_GRID {
        if (to - base.min_probability).abs() > 1e-9 {
            out.push(("min_probability", base.min_probability, to, Thresholds { min_probability: to, ..*base }));
        }
    }
    for to in MARGIN_GRID {
        if (to - base.min_margin).abs() > 1e-9 {
            out.push(("min_margin", base.min_margin, to, Thresholds { min_margin: to, ..*base }));
        }
    }
    out
}

/// A proposal, or why there is none. `feedback` are the person's answers (an answer with no model answer kept, or no label, cannot tell anything about a threshold and is left out);
/// `held_out` the held-out requests; `base` the thresholds as they are.
pub fn evaluate(feedback: &[FeedbackRow], held_out: &[HeldOut], base: &Thresholds) -> Result<Proposal, NoProposal> {
    let from_people: Vec<Example> = latest_answers(feedback, 0)
        .into_iter()
        .filter_map(|row| {
            let evidence = row.evidence.clone()?;
            evidence.model.as_ref()?;
            Some(Example { evidence, expect: row.correct? })
        })
        .collect();
    if from_people.len() < MIN_FEEDBACK {
        return Err(NoProposal::new(
            "too_few_feedback",
            format!("{} answers of the person have the model's answer kept with them; at least {MIN_FEEDBACK} are needed (the router has to be on for the answer to be kept)", from_people.len()),
        ));
    }
    let held: Vec<Example> = held_out.iter().filter(|h| h.evidence.model.is_some()).map(|h| Example { evidence: h.evidence.clone(), expect: h.expect }).collect();
    if held.len() < MIN_HELD_OUT {
        return Err(NoProposal::new(
            "too_few_held_out",
            format!("{} of the {} held-out requests have the model's answer recorded; at least {MIN_HELD_OUT} are needed to hold a change against (run the live measurement of core/tests/decision_call_test.rs with DECISION_REPORT and keep its report as core/tests/fixtures/routing_held_out.json)", held.len(), held_out.len()),
        ));
    }
    let (people_now, held_now) = (right(&from_people, base), right(&held, base));
    struct Best {
        name: &'static str,
        from: f64,
        to: f64,
        people: usize,
        held: usize,
    }
    let mut best: Option<Best> = None;
    let mut hurts_held_out = false;
    for (name, from, to, tried) in candidates(base) {
        let people = right(&from_people, &tried);
        // Whole answers, not fractions: "5 more of every 100" is a comparison of integers.
        if people <= people_now || (people - people_now) * 100 < FEEDBACK_MARGIN_POINTS * from_people.len() {
            continue;
        }
        let held_after = right(&held, &tried);
        if held_after < held_now {
            hurts_held_out = true;
            continue;
        }
        // The better change has more answers right, then more held-out right, then is the smaller step.
        let better = match &best {
            None => true,
            Some(b) => (people, held_after) > (b.people, b.held) || ((people, held_after) == (b.people, b.held) && (to - from).abs() < (b.to - b.from).abs()),
        };
        if better {
            best = Some(Best { name, from, to, people, held: held_after });
        }
    }
    match best {
        Some(b) => Ok(Proposal {
            name: b.name.to_string(),
            from: b.from,
            to: b.to,
            held_out_before: held_now as f64 / held.len() as f64,
            held_out_after: b.held as f64 / held.len() as f64,
            n_held_out: held.len(),
            feedback_before: people_now as f64 / from_people.len() as f64,
            feedback_after: b.people as f64 / from_people.len() as f64,
            n_feedback: from_people.len(),
        }),
        None if hurts_held_out => Err(NoProposal::new("hurts_held_out", "a change would route more of the person's answers right, but every such change routes a held-out request worse".into())),
        None => Err(NoProposal::new("no_gain", format!("no change to a threshold routes {FEEDBACK_MARGIN_POINTS} more of every hundred of the person's answers right"))),
    }
}

/// The proposal for the thresholds the router has now, or none.
pub fn propose_threshold(feedback: &[FeedbackRow], held_out: &[HeldOut]) -> Option<Proposal> {
    evaluate(feedback, held_out, &thresholds()).ok()
}

/// The model's answer as a held-out file gives it: its own four numbers, or a row of the report that `decision_call_test` writes (`{"route": .., "detail": {..}}`).
#[derive(Deserialize)]
struct RawModel {
    index: Option<usize>,
    probability: Option<f64>,
    margin: Option<f64>,
    covered: Option<f64>,
    detail: Option<Box<RawModel>>,
}

impl RawModel {
    fn answer(&self) -> Option<ModelAnswer> {
        if let Some(inner) = &self.detail {
            return inner.answer();
        }
        Some(ModelAnswer { index: self.index?, probability: self.probability?, margin: self.margin?, covered: self.covered? })
    }
}

#[derive(Deserialize)]
struct HeldOutRow {
    id: String,
    message: String,
    #[serde(alias = "expect")]
    expect_route: String,
    #[serde(default)]
    model: Option<RawModel>,
    /// The gate's route and confidence as the measurement saw them; without them the rule gate is asked again.
    #[serde(default)]
    gate: Option<String>,
    #[serde(default)]
    gate_confidence: Option<f64>,
}

/// The held-out requests of a file (rows with a label that is not a route are left out). The gate's decision is the measurement's own where the row has it, otherwise the rule gate's:
/// the gate that decides when the model gate is off, which is how Aqua is run.
pub fn held_out_from_json(json: &str) -> Vec<HeldOut> {
    let rows: Vec<HeldOutRow> = serde_json::from_str(json).unwrap_or_default();
    let rule_gate = LayaGate::disabled_heuristic(std::path::PathBuf::new());
    rows.into_iter()
        .filter_map(|row| {
            let expect = GateRoute::parse(&row.expect_route)?;
            let (rules_route, rules_confidence) = match (row.gate.as_deref().and_then(GateRoute::parse), row.gate_confidence) {
                (Some(route), Some(confidence)) => (route, confidence),
                _ => {
                    let d = rule_gate.decide(&row.message, true);
                    (d.route, d.confidence)
                }
            };
            Some(HeldOut { id: row.id, expect, evidence: Evidence { rules_route, rules_confidence, mcp_available: true, model: row.model.as_ref().and_then(RawModel::answer) } })
        })
        .collect()
}

/// The held-out requests the build carries.
pub fn builtin_held_out() -> Vec<HeldOut> {
    held_out_from_json(FIXTURE)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: Thresholds = Thresholds { min_probability: 0.98, min_margin: 0.9, min_covered: 0.5 };

    fn evidence(rules: GateRoute, confidence: f64, answer: Option<(usize, f64)>) -> Evidence {
        Evidence { rules_route: rules, rules_confidence: confidence, mcp_available: true, model: answer.map(|(index, probability)| ModelAnswer { index, probability, margin: 0.95, covered: 0.97 }) }
    }

    fn answer_row(id: u64, correct: GateRoute, evidence: Evidence) -> FeedbackRow {
        FeedbackRow { at: 1, text: format!("request {id}"), button: "chat".into(), correct: Some(correct), chosen: Some(evidence.rules_route), decided_at_ms: id, evidence: Some(evidence), ..Default::default() }
    }

    /// `hard` answers the router was not sure enough of at 0.98 (the model said tools at 0.96 and was right; the rules said chat), then `fine` that every threshold gets right.
    fn people(hard: u64, fine: u64) -> Vec<FeedbackRow> {
        let mut rows = Vec::new();
        for i in 0..hard {
            rows.push(answer_row(i + 1, GateRoute::McpAgent, evidence(GateRoute::Chat, 0.2, Some((2, 0.96)))));
        }
        for i in 0..fine {
            rows.push(answer_row(1000 + i, GateRoute::McpAgent, evidence(GateRoute::McpAgent, 0.2, Some((2, 0.995)))));
        }
        rows
    }

    /// 30 held-out requests that no threshold in the grid routes wrongly: the model agrees with the rules and is sure.
    fn held_out() -> Vec<HeldOut> {
        let mut rows = Vec::new();
        for i in 0..15 {
            rows.push(HeldOut { id: format!("chat-{i}"), expect: GateRoute::Chat, evidence: evidence(GateRoute::Chat, 0.2, Some((1, 0.995))) });
            rows.push(HeldOut { id: format!("tools-{i}"), expect: GateRoute::McpAgent, evidence: evidence(GateRoute::McpAgent, 0.2, Some((2, 0.99))) });
        }
        rows
    }

    #[test]
    fn replay_is_the_routers_own_combine_with_the_thresholds_given() {
        // The rules say chat, unsure; the model says tools at 0.96: held back at 0.98, let through at 0.95.
        let ev = evidence(GateRoute::Chat, 0.2, Some((2, 0.96)));
        assert_eq!(replay(&ev, &BASE), GateRoute::Chat);
        assert_eq!(replay(&ev, &Thresholds { min_probability: 0.95, ..BASE }), GateRoute::McpAgent);
        // Rules that are certain stand against any model.
        assert_eq!(replay(&evidence(GateRoute::Chat, 1.0, Some((2, 0.999))), &Thresholds { min_probability: 0.8, ..BASE }), GateRoute::Chat);
        // A model that was not asked changes nothing.
        assert_eq!(replay(&evidence(GateRoute::Chat, 0.2, None), &Thresholds { min_probability: 0.8, ..BASE }), GateRoute::Chat);
        // No tools: a task cannot go to them.
        let no_tools = Evidence { mcp_available: false, ..ev.clone() };
        assert_eq!(replay(&no_tools, &Thresholds { min_probability: 0.95, ..BASE }), GateRoute::Chat);
        // The real thresholds are the ones the grid is built around.
        assert_eq!((thresholds().min_probability, thresholds().min_margin, thresholds().min_covered), (BASE.min_probability, BASE.min_margin, BASE.min_covered));
    }

    #[test]
    fn a_change_that_helps_the_answers_and_does_not_hurt_the_held_out_requests_is_proposed_with_the_numbers() {
        let p = evaluate(&people(4, 16), &held_out(), &BASE).expect("a proposal");
        // Four of the twenty answers were wrong at 0.98; every value from 0.80 to 0.96 gets them right, and 0.96 is the smallest step.
        assert_eq!((p.name.as_str(), p.from, p.to), ("min_probability", 0.98, 0.96));
        assert_eq!((p.n_feedback, p.n_held_out), (20, 30));
        assert_eq!((p.feedback_before, p.feedback_after), (16.0 / 20.0, 1.0));
        assert_eq!((p.held_out_before, p.held_out_after), (1.0, 1.0));
        // The same through the function that uses the router's own thresholds.
        assert_eq!(propose_threshold(&people(4, 16), &held_out()), Some(p));
    }

    #[test]
    fn exactly_five_more_of_every_hundred_is_enough_and_one_short_of_it_is_not() {
        // 20 answers, one gained: 1 of 20 is 5 points.
        assert!(evaluate(&people(1, 19), &held_out(), &BASE).is_ok());
        // 40 answers, one gained: 2.5 points; two gained would be 5.
        let none = evaluate(&people(1, 39), &held_out(), &BASE).unwrap_err();
        assert_eq!(none.code, "no_gain", "{none:?}");
        assert!(evaluate(&people(2, 38), &held_out(), &BASE).is_ok());
    }

    #[test]
    fn no_proposal_when_the_answers_do_not_improve_by_the_margin() {
        // Every answer is already routed right.
        let none = evaluate(&people(0, 20), &held_out(), &BASE).unwrap_err();
        assert_eq!(none.code, "no_gain", "{none:?}");
        assert_eq!(propose_threshold(&people(0, 20), &held_out()), None);
    }

    #[test]
    fn no_proposal_when_a_change_that_helps_the_answers_routes_a_held_out_request_worse() {
        let mut held = held_out();
        // Three requests the rules route right and a model at 0.965 would send to the tools wrongly: held back at 0.98, let through at every value that would help the answers.
        for i in 0..3 {
            held.push(HeldOut { id: format!("trap-{i}"), expect: GateRoute::Chat, evidence: evidence(GateRoute::Chat, 0.2, Some((2, 0.965))) });
        }
        let none = evaluate(&people(4, 16), &held, &BASE).unwrap_err();
        assert_eq!(none.code, "hurts_held_out", "{none:?}");
        assert_eq!(propose_threshold(&people(4, 16), &held), None);
        // Without the traps the same answers do get a proposal: the held-out set is what said no.
        assert!(evaluate(&people(4, 16), &held_out(), &BASE).is_ok());
    }

    #[test]
    fn a_change_is_chosen_among_those_that_do_not_hurt_even_when_a_bigger_one_would() {
        let mut held = held_out();
        // A trap at 0.94 hurts every value up to 0.94 and none above it: the answers at 0.96 are still won by 0.96 and 0.95.
        for i in 0..3 {
            held.push(HeldOut { id: format!("trap-{i}"), expect: GateRoute::Chat, evidence: evidence(GateRoute::Chat, 0.2, Some((2, 0.94))) });
        }
        let p = evaluate(&people(4, 16), &held, &BASE).expect("a proposal");
        assert_eq!((p.name.as_str(), p.to), ("min_probability", 0.96));
        // All 33 are routed right as the router is and with the change: the traps are held back by 0.96 as they are by 0.98.
        assert_eq!((p.n_held_out, p.held_out_before, p.held_out_after), (33, 1.0, 1.0));
    }

    #[test]
    fn too_few_answers_or_too_few_held_out_requests_with_the_models_answer_is_no_proposal_and_says_how_many() {
        let few = evaluate(&people(4, 15), &held_out(), &BASE).unwrap_err();
        assert_eq!(few.code, "too_few_feedback");
        assert!(few.reason.starts_with("19 answers"), "{}", few.reason);
        // Answers without the model's answer kept do not count.
        let mut blind = people(4, 16);
        for row in blind.iter_mut().take(10) {
            row.evidence.as_mut().unwrap().model = None;
        }
        assert_eq!(evaluate(&blind, &held_out(), &BASE).unwrap_err().code, "too_few_feedback");
        // Nor do answers with no evidence at all (the rows of an early version), or two answers for one decision.
        let mut early = people(4, 16);
        early[0].evidence = None;
        assert_eq!(evaluate(&early, &held_out(), &BASE).unwrap_err().code, "too_few_feedback");
        let mut twice = people(4, 16);
        twice[1].decided_at_ms = twice[0].decided_at_ms;
        assert_eq!(evaluate(&twice, &held_out(), &BASE).unwrap_err().code, "too_few_feedback");

        let mut short = held_out();
        short.pop();
        let none = evaluate(&people(4, 16), &short, &BASE).unwrap_err();
        assert_eq!(none.code, "too_few_held_out");
        assert!(none.reason.starts_with("29 of the 29 held-out"), "{}", none.reason);
        // Held-out requests with no model answer do not count either.
        let mut unanswered = held_out();
        for h in unanswered.iter_mut().take(5) {
            h.evidence.model = None;
        }
        assert_eq!(evaluate(&people(4, 16), &unanswered, &BASE).unwrap_err().code, "too_few_held_out");
    }

    #[test]
    fn a_held_out_file_is_read_in_its_own_shape_and_in_the_shape_of_the_measurements_report() {
        let json = r#"[
            {"id":"a","message":"open notepad","expect_route":"mcp_agent","gate":"chat","gate_confidence":0.2,"model":{"index":2,"probability":0.99,"margin":0.98,"covered":0.97}},
            {"id":"b","message":"hello","expect":"chat","gate":"chat","gate_confidence":0.9,"model":{"route":"chat","detail":{"index":1,"probability":0.995,"margin":0.99,"covered":0.99,"shares":[0.0,0.995,0.005]}}},
            {"id":"c","message":"x","expect_route":"chat","gate":"chat","gate_confidence":0.9,"model":null},
            {"id":"d","message":"y","expect_route":"nonsense","gate":"chat","gate_confidence":0.9}
        ]"#;
        let rows = held_out_from_json(json);
        assert_eq!(rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["a", "b", "c"], "a label that is not a route is left out");
        assert_eq!(rows[0].evidence, Evidence { rules_route: GateRoute::Chat, rules_confidence: 0.2, mcp_available: true, model: Some(ModelAnswer { index: 2, probability: 0.99, margin: 0.98, covered: 0.97 }) });
        assert_eq!(rows[1].evidence.model.map(|m| (m.index, m.probability, m.margin, m.covered)), Some((1, 0.995, 0.99, 0.99)));
        assert_eq!(rows[2].evidence.model, None);
        assert!(held_out_from_json("not json").is_empty());
    }

    #[test]
    fn the_held_out_requests_the_build_carries_are_the_labelled_requests_of_the_gates_fixtures() {
        let rows = builtin_held_out();
        assert!(rows.len() >= 130, "{} rows", rows.len());
        let mut ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), rows.len(), "every id once");
        // Each is a request of one of the four sets the router was measured on, with the same label.
        #[derive(Deserialize)]
        struct Case {
            id: String,
            expect_route: String,
        }
        let sets = [
            include_str!("../../tests/fixtures/laya_gate_cases.json"),
            include_str!("../../tests/fixtures/laya_gate_holdout.json"),
            include_str!("../../tests/fixtures/laya_gate_holdout2.json"),
            include_str!("../../tests/fixtures/laya_gate_decision_cases.json"),
        ];
        let known: Vec<Case> = sets.iter().flat_map(|s| serde_json::from_str::<Vec<Case>>(s).expect("a fixture set")).collect();
        for row in &rows {
            let case = known.iter().find(|c| c.id == row.id).unwrap_or_else(|| panic!("{} is in none of the four sets", row.id));
            assert_eq!(GateRoute::parse(&case.expect_route), Some(row.expect), "{}", row.id);
        }
    }

    #[test]
    fn a_proposal_changes_nothing_the_thresholds_are_the_same_after_it() {
        let before = thresholds();
        let _ = propose_threshold(&people(4, 16), &held_out());
        let after = thresholds();
        assert_eq!((before.min_probability, before.min_margin, before.min_covered), (after.min_probability, after.min_margin, after.min_covered));
        assert_eq!(before.min_probability, super::super::MOVE_PROBABILITY);
    }
}

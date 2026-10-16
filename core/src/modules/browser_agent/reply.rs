//! What the planner said and what a run ends with: the reader of its reply (a call, several calls, a question, an answer), the one place an answer ends up with its citations, and the words of a run that was not answered (card OA3-05, move only).

use super::*;

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(super) enum LlmAction {
    Tool {
        tool: String,
        #[serde(default)]
        arguments: Value,
    },
    Final {
        final_answer: String,
    },
    FinalAlt {
        #[serde(rename = "final")]
        final_text: String,
    },
    /// Several calls in one reply (`{"actions": [...]}`): the loop runs the first and keeps the others for the steps that follow, as
    /// long as each one worked and left the next one's footing as it was.
    Batch(Vec<(String, Value)>),
    /// A question for the person instead of a guess (`{"ask": "..."}`).
    Ask { question: String, choices: Vec<String> },
    /// An earlier result asked for again (`{"recall": "h3"}`, the context manager).
    Recall { handle: String },
}

/// What the loop knows about the call it has just made, to decide whether the calls the planner chained after it may still run.
pub(super) struct LastCall {
    pub(super) failed: bool,
    /// The tool host made the call (a file, terminal or git tool), not the browser or the desktop.
    pub(super) host: bool,
    /// The person was asked about it.
    pub(super) asked: bool,
    pub(super) tool: String,
}

/// May the next call the planner chained go ahead? Only after a call that worked, was not asked about, and did not change what the next call
/// was written for. A host tool (a file read, a command) changes nothing the next call stands on; a click or a navigation changes the page
/// whose element handles the next call was written against, so the planner must look again. Reading the page changes nothing either.
pub(super) fn batch_may_continue(last: &LastCall) -> bool {
    !last.failed && !last.asked && (last.host || is_observe_tool(&last.tool))
}

pub(super) const BATCH_NOT_RUN: &str = "Not executed: an earlier action in this round failed, was refused or changed the page. Plan it again if it is still needed.";

pub(super) fn extract_json_object(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        if v.is_object() {
            return Some(trimmed.to_string());
        }
    }
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        let after = after
            .strip_prefix("json")
            .or_else(|| after.strip_prefix("JSON"))
            .unwrap_or(after)
            .trim_start();
        if let Some(end) = after.find("```") {
            let inner = after[..end].trim();
            if serde_json::from_str::<Value>(inner).is_ok() {
                return Some(inner.to_string());
            }
        }
    }
    if let Some(i) = trimmed.find('{') {
        let mut depth = 0i32;
        // The braces inside a string of the call are text, not the call's own: the code a model writes in an argument (`fn main() {`) has them, balanced or not. Before this a call
        // like that, written after a few words, was taken for the model's answer (found by `parse_properties_tests`).
        let (mut in_string, mut escaped) = (false, false);
        for (j, ch) in trimmed[i..].char_indices() {
            if in_string {
                match ch {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '"' => in_string = false,
                    _ => {}
                }
                continue;
            }
            match ch {
                '"' => in_string = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        let candidate = &trimmed[i..i + j + 1];
                        if serde_json::from_str::<Value>(candidate).is_ok() {
                            return Some(candidate.to_string());
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// An answer as text. The model sometimes writes `{"final":42}` for "reply with only the number".
fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

pub(super) fn parse_action(llm_text: &str) -> Result<LlmAction> {
    let cleaned = crate::modules::llm_router::strip_reasoning_tags(llm_text);
    let json_str = match extract_json_object(&cleaned) {
        Some(j) => j,
        None => return Ok(LlmAction::Final { final_answer: cleaned.to_string() }),
    };
    let value: Value = serde_json::from_str(&json_str)?;
    if let Some(final_text) = value.get("final").and_then(scalar_text) {
        return Ok(LlmAction::FinalAlt {
            final_text: crate::modules::llm_router::strip_reasoning_tags(&final_text),
        });
    }
    if let Some(final_text) = value.get("final_answer").and_then(scalar_text) {
        return Ok(LlmAction::Final {
            final_answer: crate::modules::llm_router::strip_reasoning_tags(&final_text),
        });
    }
    if let Some(tool) = value.get("tool").and_then(|v| v.as_str()) {
        return Ok(LlmAction::Tool {
            tool: tool.to_string(),
            arguments: value.get("arguments").cloned().unwrap_or(json!({})),
        });
    }
    if let Some(actions) = value.get("actions").and_then(|v| v.as_array()) {
        let calls: Vec<(String, Value)> = actions
            .iter()
            .filter_map(|a| Some((a.get("tool")?.as_str()?.to_string(), a.get("arguments").cloned().unwrap_or(json!({})))))
            .take(crate::modules::planner::schema::max_batch())
            .collect();
        if calls.is_empty() {
            bail!("JSON action has an empty or malformed actions list: {json_str}");
        }
        return Ok(LlmAction::Batch(calls));
    }
    if let Some(question) = value.get("ask").and_then(scalar_text).filter(|q| !q.trim().is_empty()) {
        let choices = value.get("choices").and_then(|c| c.as_array()).map(|c| c.iter().filter_map(scalar_text).take(4).collect()).unwrap_or_default();
        return Ok(LlmAction::Ask { question: crate::modules::llm_router::strip_reasoning_tags(&question), choices });
    }
    if let Some(handle) = value.get("recall").and_then(scalar_text).filter(|h| !h.trim().is_empty()) {
        return Ok(LlmAction::Recall { handle: handle.trim().chars().take(8).collect() });
    }
    bail!("JSON action missing tool/final fields: {json_str}");
}

/// The words of a reply that could not be read as a tool call or a final. A JSON object that holds the answer in one text field (`answer`, `text`, `response`, `reply`, `message`) is
/// shown as that text, so the person does not read braces and quotes and each sentence of it can be checked; any other reply is returned as it is.
pub(super) fn words_of_unreadable_reply(reply: &str) -> String {
    let cleaned = crate::modules::llm_router::strip_reasoning_tags(reply);
    if let Some(value) = extract_json_object(&cleaned).and_then(|j| serde_json::from_str::<Value>(&j).ok()) {
        for key in ["answer", "text", "response", "reply", "message"] {
            if let Some(words) = value.get(key).and_then(|v| v.as_str()).map(str::trim).filter(|w| !w.is_empty()) {
                return words.to_string();
            }
        }
    }
    reply.to_string()
}

/// The one place a model-written answer ends up in before it is returned (card R6-18): what the run read on the web checks it (a cited claim must be in the source it cites, a claim
/// with no citation is cited from the source that says it, or marked), the metrics of the check are written, a sentence says how old the newest cited source is, and the answer ends with
/// the list of the sources it used. With nothing read on the web the answer comes back as it is. Every final that is the model's own words goes through here; the answers Aqua composes
/// itself from a page (the scripted research and headline paths) and the summary of a run that ran out of rounds cite nothing and are not checked.
pub(super) fn finalize_cited_answer(run: &str, answer: &str) -> (String, Vec<Value>) {
    let clean = crate::modules::llm_router::strip_reasoning_tags(answer);
    let checked = crate::sources::check_answer(run, &clean);
    // The rate is read with the threshold it was made with: both are written down for every answer that was checked.
    if checked.supported + checked.unsupported + checked.unsourced > 0 {
        info!("{}", checked.summary());
        crate::metrics::add("aqua_answer_claims_total", &[("verdict", "supported")], checked.supported as f64);
        crate::metrics::gauge("aqua_verifier_name_share_percent", &[], checked.name_share_percent as f64);
    }
    if checked.unsupported + checked.unsourced > 0 {
        crate::metrics::add("aqua_answer_claims_total", &[("verdict", "unsupported")], checked.unsupported as f64);
        crate::metrics::add("aqua_answer_claims_total", &[("verdict", "unsourced")], checked.unsourced as f64);
    }
    let age = crate::sources::data_age_sentence(run, &checked.cited);
    let age = if age.is_empty() { String::new() } else { format!("\n\n{age}") };
    (format!("{}{age}{}", checked.text, crate::sources::sources_block(run, &checked.cited)), crate::sources::sources_json(run, &checked.cited))
}

/// The answer for a run that stopped before the model gave one (its steps ran out, or the model failed): what it found, and the plain
/// statement that it did not finish. It used to be `Regarding "<goal>": <the first lines of the last result>`, which reads as the answer to the
/// question, and for a coding task it was the last output of a test run that was still failing.
pub(super) fn synthesize_unfinished(goal: &str, history: &[(String, String)]) -> String {
    let said = synthesize_from_history(goal, history);
    match said.strip_prefix(&format!("Regarding \"{goal}\": ")) {
        Some(found) => format!("I ran out of steps before I could finish \"{goal}\", so I cannot tell you it is done. What I found so far: {found}"),
        // The other endings say already that it was not completed, or that nothing reliable was found.
        None => said,
    }
}

pub(super) fn synthesize_from_history(goal: &str, history: &[(String, String)]) -> String {
    if !observations_cover_goal(goal, history) {
        let fail_bits: Vec<_> = history
            .iter()
            .filter(|(_, o)| observation_indicates_failure(o))
            .map(|(t, o)| {
                format!(
                    "{t}: {}",
                    o.lines()
                        .next()
                        .unwrap_or(o.as_str())
                        .chars()
                        .take(100)
                        .collect::<String>()
                )
            })
            .take(3)
            .collect();
        if fail_bits.is_empty() {
            return format!(
                "I could not verify that \"{goal}\" completed. No positive observation showed the target open."
            );
        }
        let hint = if goal_asks_for_information(goal) {
            "The pages that were opened were not about this question."
        } else if history.iter().any(|(t, o)| observation_indicates_failure(o) && (t.eq_ignore_ascii_case("App") || t.starts_with("local_open"))) {
            "Try opening it in the browser if the app is not installed."
        } else {
            "The page was not what the steps needed: open it yourself to see what it asks for (a sign-in, a pop-up), then ask again."
        };
        return format!("I could not complete \"{goal}\". Failures: {}. {hint}", fail_bits.join(" | "));
    }
    let Some((_, obs)) = history
        .iter()
        .rev()
        .find(|(_, o)| !observation_indicates_failure(o) && o.trim().len() > 8)
    else {
        return history
            .last()
            .map(|(_, o)| o.clone())
            .unwrap_or_else(|| "I could not complete that task with the available tools.".into());
    };
    // The tool host wraps what came from outside in a fence and a reminder for the model. They are not part of what was found.
    let unfenced = without_fence(obs);
    let obs = unfenced.as_str();
    let cleaned = format_live_observation(obs).unwrap_or_else(|| {
        obs.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("Status Code:"))
            .map(|l| l.trim_start_matches("Response:").trim())
            .filter(|l| !l.is_empty())
            .take(6)
            .collect::<Vec<_>>()
            .join(" ")
    });
    if goal_is_news(goal) {
        let lines = headline_lines(obs);
        if !lines.is_empty() {
            return format!("Regarding \"{goal}\":\n{}", lines.join("\n"));
        }
    }
    // A desktop or page dump is not an answer, and a desktop one lists the titles of every open
    // window. Say plainly that nothing reliable was found instead of showing it.
    if looks_like_state_dump(&cleaned) {
        if let Some(answer) = research_page_answer(goal, history) {
            return answer;
        }
        let headlines = headline_lines(obs);
        if goal_is_news(goal) && !headlines.is_empty() {
            return format!("Regarding \"{goal}\":\n{}", headlines.join("\n"));
        }
        if goal_is_news(goal) {
            return format!(
                "I could not get a reliable answer for \"{goal}\". The pages I opened did not include a list of headlines I could quote, so I have not guessed."
            );
        }
        if crate::laya_validator::needs_web_lookup(goal) && !crate::laya_validator::looks_live_info(goal) {
            return format!(
                "I could not get a reliable answer for \"{goal}\". I opened the pages, but they did not include a description I could quote."
            );
        }
        return format!(
            "I could not get a reliable answer for \"{goal}\". I read the pages I opened but did not find figures I could \
             verify, so I have not guessed. Try asking again, or narrow it (for example \"Nifty 50 level today\")."
        );
    }
    if cleaned.is_empty() {
        format!("I looked that up for \"{goal}\" but got an empty tool result.")
    } else {
        format!("Regarding \"{goal}\": {cleaned}")
    }
}

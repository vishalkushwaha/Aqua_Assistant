//! The checks on a final answer before a run may end with it: that what it says is in what the run read (the words of the goal, the figures, the temperatures and conditions), that it is not a promise or a refusal dressed as an answer, and that a coding run showed a passing check (card OA3-05, move only).

use super::*;

pub(crate) fn last_result_is_verified_observation(history: &[(String, String)]) -> bool {
    let Some((tool, obs)) = history.last() else {
        return false;
    };
    if observation_indicates_failure(obs) {
        return false;
    }
    if is_observe_tool(tool) || crate::agent_tools::is_host_tool(crate::agent_tools::host().map(|h| h.as_ref()), tool) {
        return !is_action_ack_only(obs) || obs.len() > 40;
    }
    // Rich shell/API payloads count as observations (weather curl, etc.).
    !is_action_ack_only(obs) && obs.trim().len() > 40
}

pub(super) fn needs_observation_before_final(history: &[(String, String)]) -> bool {
    let Some((tool, obs)) = history.last() else {
        return false;
    };
    if observation_indicates_failure(obs) {
        return true;
    }
    // A host tool's result is its own proof (exit code, bytes written, rows); there is no screen to look at.
    if crate::agent_tools::is_host_tool(crate::agent_tools::host().map(|h| h.as_ref()), tool) {
        return false;
    }
    if is_observe_tool(tool) {
        return false;
    }
    is_action_ack_only(obs) || tool.eq_ignore_ascii_case("App")
}

fn positive_history<'a>(history: &'a [(String, String)]) -> Vec<&'a (String, String)> {
    history
        .iter()
        .filter(|(_, o)| !observation_indicates_failure(o))
        .collect()
}

fn answer_claims_success(answer: &str) -> bool {
    let a = answer.to_ascii_lowercase();
    [
        "successfully",
        "opened successfully",
        "launched successfully",
        "is now open",
        "is open",
        "has been opened",
        "has been launched",
        "opened youtube",
        "youtube is open",
        "task completed",
        "completed successfully",
        "done successfully",
    ]
    .iter()
    .any(|p| a.contains(p))
}

pub(super) fn significant_goal_tokens(goal: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "tell", "me", "please", "the", "a", "an", "and", "then", "to", "for", "of", "in",
        "on", "at", "my", "open", "launch", "start", "run", "show", "get", "make", "with",
        "using", "into", "from", "now", "today", "todays", "just", "can", "you", "will",
        // Question and function words describe what is being asked, not a fact a page must repeat:
        // "What is the weather in Pune" needs Pune in the result, not the word "what".
        "what", "whats", "which", "who", "whom", "when", "where", "why", "how", "are", "was", "were",
        "does", "did", "has", "have", "had", "that", "this", "there", "their", "about", "give", "find",
        "check", "could", "would", "should", "much", "many", "any", "some", "our", "your", "its",
        "right", "currently", "tonight", "tomorrow", "yesterday", "like", "going", "been",
    ];
    goal.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() >= 3 && !STOP.contains(t))
        .map(|t| t.to_string())
        .collect()
}

/// Topic / ranking words that describe *what kind* of data was asked for rather than an entity
/// that must literally appear in it: "top 5 news" is satisfied by five headlines, which rarely
/// contain the words "top" or "news". Satisfied by a rich non-ack observation (e.g. RSS, wttr).
const SOFT_TOPICS: &[&str] = &[
    "weather",
    "temperature",
    "forecast",
    "news",
    "headline",
    "headlines",
    "top",
    "latest",
    "recent",
    "current",
    "breaking",
    "trending",
    "live",
    "status",
    "info",
    "information",
];

/// Words of a spoken request that no page or window will repeat, so they must never be demanded as evidence: greetings and
/// fillers that came out of the speech recognition ("hello everyone", "yeah"), and words that say where in the answer
/// something sits ("what window is in front" asks for a name, not for the word "front"). Wrongly demanding them made a correct
/// answer fail the check again and again until the run ran out of time.
const EVIDENCE_FILLER: &[&str] = &[
    "hey", "aqua", "hello", "hiya", "yeah", "yes", "yep", "nope", "okay", "thanks", "thank", "sorry", "pardon", "everyone", "everybody", "guys",
    "well", "actually", "maybe", "really", "basically", "kindly", "talking", "front", "foreground", "background", "active",
];

/// The words of the goal that the results must contain before an answer is accepted: what is left after the question and
/// function words, the topic words ("news") and the fillers of speech.
pub(super) fn hard_goal_tokens(goal: &str) -> Vec<String> {
    significant_goal_tokens(goal)
        .into_iter()
        .filter(|t| !SOFT_TOPICS.contains(&t.as_str()) && !EVIDENCE_FILLER.contains(&t.as_str()) && !crate::memory::solutions_db::is_filler_word(t))
        .collect()
}

/// Is `token` (a word of the goal) a word of `text` (lower case)? Whole words, or the same word with another ending
/// ("headline" and "headlines", "file" and "files"); never a piece of another word ("hey" is not in "they").
pub(super) fn evidence_has_word(text: &str, token: &str) -> bool {
    // A web address spells a space as %20 ("stock%20market"); that is a separator, not part of the next word.
    let mut plain = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && bytes.get(i + 1).is_some_and(u8::is_ascii_hexdigit) && bytes.get(i + 2).is_some_and(u8::is_ascii_hexdigit) {
            plain.push(' ');
            i += 3;
        } else {
            let ch = text[i..].chars().next().unwrap_or(' ');
            plain.push(ch);
            i += ch.len_utf8();
        }
    }
    plain.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).any(|w| {
        w == token
            || w.strip_suffix('s') == Some(token)
            || token.strip_suffix('s') == Some(w)
            || (w.len() >= 4 && token.len() >= 4 && (w.starts_with(token) || token.starts_with(w)))
    })
}

pub(crate) fn observations_cover_goal(goal: &str, history: &[(String, String)]) -> bool {
    if significant_goal_tokens(goal).is_empty() {
        return true;
    }
    let hard = hard_goal_tokens(goal);
    // Failure text like "Youtube not found" must NOT count as evidence.
    let positive = positive_history(history);
    if positive.is_empty() {
        return false;
    }
    let blob = positive
        .iter()
        .map(|(tool, obs)| format!("{tool} {obs}"))
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    if hard.is_empty() {
        return last_result_is_verified_observation(history);
    }
    if hard.iter().all(|t| evidence_has_word(&blob, t)) {
        return true;
    }
    // The goal words ("read", "folder", "workspace") describe what to do, not what the result says: a
    // file's contents or a command's output will not repeat them. A host tool that succeeded and
    // returned something is its own evidence; the model's answer is then checked by the other rules.
    let host = crate::agent_tools::host().map(|h| h.as_ref());
    // (What a helper answered for the run is evidence in the same way, and a helper that stopped without an answer is none: it read through the host, in a run of its own, and said where; card OA11-04.)
    positive.iter().any(|(tool, obs)| (crate::agent_tools::is_host_tool(host, tool) || (tool == child::TOOL && child::is_a_helper_answer(obs))) && !obs.trim().is_empty())
}

fn final_claims_future_work(answer: &str) -> bool {
    let a = answer.to_ascii_lowercase();
    [
        "i will now",
        "i'll now",
        "i will next",
        "next i will",
        "going to navigate",
        "will navigate",
        "will now",
        "let me now",
        "i am going to",
        "about to open",
        "then i will",
    ]
    .iter()
    .any(|p| a.contains(p))
}

/// "top 5 news headlines retrieved successfully" — a short report *about* fetching data
/// instead of the data. Longer answers that happen to say "retrieved" are left alone.
fn final_only_claims_retrieval(answer: &str) -> bool {
    let a = answer.trim().to_ascii_lowercase();
    a.chars().count() < 120
        && ["retrieved", "fetched", "obtained", "collected", "gathered"]
            .iter()
            .any(|w| a.contains(w))
}

/// Returns rejection reason if this final must not end the agent loop.
/// Text an 8B model sometimes copies out of the instructions instead of answering.
pub(super) fn is_placeholder_final(answer: &str) -> bool {
    crate::memory::solutions_db::is_placeholder_answer(answer)
}

/// A goal that asks for information (a question or "tell me / show me / what is …"), as opposed to
/// asking for an action. Its final answer must contain the information.
pub(crate) fn goal_asks_for_information(goal: &str) -> bool {
    let g = goal.trim().to_ascii_lowercase();
    let first = g.split(|c: char| !c.is_ascii_alphanumeric()).find(|w| !w.is_empty()).unwrap_or("");
    crate::laya_validator::looks_live_info(goal)
        || ["what", "whats", "which", "who", "when", "where", "how", "why", "tell", "show", "give", "list", "find", "check", "read", "summarize", "summarise", "explain"].contains(&first)
        || g.contains("tell me")
        || g.contains("show me")
}

/// Weather words that mean the same thing on a page: claiming any of a group is supported when
/// the page uses any word of that group.
const CONDITION_GROUPS: &[&[&str]] = &[
    &["rain", "shower", "drizzle"],
    &["thunder", "storm"],
    &["snow", "sleet", "hail"],
    &["fog", "mist", "haze", "smoke"],
    &["sunny", "clear", "sun "],
    &["cloud", "overcast"],
];

/// The reading in a search page's current-weather widget: a heading like `"33 °Celsius °Fahrenheit"`.
/// (`blob` is lower-case.) Forecast highs and lows elsewhere on the page are different numbers.
fn current_temperature_widget(blob: &str) -> Option<String> {
    let mut from = 0;
    while let Some(pos) = blob[from..].find("heading \"") {
        let start = from + pos + "heading \"".len();
        let digits: String = blob[start..].chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() && blob[start + digits.len()..].trim_start().starts_with("°celsius") {
            return Some(digits);
        }
        from = start;
    }
    None
}

/// The goal asks about today/now, not a forecast day.
fn goal_is_about_now(goal: &str) -> bool {
    let g = goal.to_ascii_lowercase();
    !["tomorrow", "yesterday", "week", "forecast", "weekend", "next ", "monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday", "hourly", "tonight"]
        .iter()
        .any(|w| g.contains(w))
}

/// The numbers the answer gives as a temperature ("28°C", "28 degrees", "28 Celsius"), each with
/// whether it is in Celsius.
fn claimed_temperatures(answer: &str) -> Vec<(String, bool)> {
    let chars: Vec<char> = answer.chars().collect();
    let mut found = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() && (i == 0 || !chars[i - 1].is_ascii_digit()) {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let number: String = chars[start..i].iter().collect();
            let after: String = chars[i..].iter().take(12).collect::<String>().to_ascii_lowercase();
            let after = after.trim_start();
            if after.starts_with('°') || after.starts_with("degree") || after.starts_with("celsius") || after.starts_with("fahrenheit") {
                let unit: String = after.chars().filter(|c| !c.is_whitespace()).take(12).collect();
                let celsius = unit.starts_with("°c") || unit.starts_with("celsius") || unit.starts_with("degreesc") || unit.starts_with("degreec");
                found.push((number, celsius));
            }
        } else {
            i += 1;
        }
    }
    found
}

/// Whether `blob` shows `number` as a temperature: "33 °Celsius", "33°C", "33 degrees".
fn blob_has_temperature(blob: &str, number: &str) -> bool {
    let mut from = 0;
    while let Some(pos) = blob[from..].find(number) {
        let at = from + pos;
        let before_ok = blob[..at].chars().next_back().is_none_or(|c| !c.is_ascii_digit());
        let rest = &blob[at + number.len()..];
        let after_ok = rest.chars().next().is_none_or(|c| !c.is_ascii_digit());
        if before_ok && after_ok {
            let tail: String = rest.chars().take(12).collect();
            let tail = tail.trim_start();
            if tail.starts_with('°') || tail.starts_with("degree") || tail.starts_with("celsius") || tail.starts_with("fahrenheit") || tail.starts_with("c ") || tail.starts_with("f ") {
                return true;
            }
        }
        from = at + number.len();
    }
    false
}

/// Facts the answer states (a temperature, a weather condition) that nothing the agent read shows.
/// The 8B model sometimes reads a chart's axis labels as the temperature, or fills in "light rain"
/// from habit, and a fluent wrong answer is worse than another look at the page.
pub(crate) fn unsupported_claims(goal: &str, answer: &str, history: &[(String, String)]) -> Vec<String> {
    let positive = positive_history(history);
    if positive.is_empty() {
        return Vec::new();
    }
    let blob = positive
        .iter()
        .map(|(_, obs)| obs.as_str())
        .collect::<Vec<_>>()
        .join("
")
        .to_lowercase();
    let mut out = Vec::new();
    let widget = if goal_is_about_now(goal) { current_temperature_widget(&blob) } else { None };
    for (number, celsius) in claimed_temperatures(answer) {
        if !blob_has_temperature(&blob, &number) {
            out.push(format!("{number} degrees"));
        } else if let (true, Some(now)) = (celsius, widget.as_ref()) {
            if *now != number {
                out.push(format!("{number} degrees (the page's current temperature reads {now}°C)"));
            }
        }
    }
    // Precise figures (an index level, a percentage, a price) must be on the page. The 8B model wrote
    // a plausible Sensex level of 72,529.07 while the page said 72,457.36. Only quoted values are
    // checked: the digits are compared without thousands separators, so "22,716.20" matches "22716.20".
    if goal_wants_figures(goal) {
        let page = blob.replace(',', "");
        for figure in precise_figures_in(answer) {
            if !page.contains(&figure) {
                out.push(format!("the figure {figure}"));
            }
        }
    }
    // A year the page never showed is the model's memory ("as of June 13, 2024"), not today's data.
    if crate::laya_validator::looks_live_info(goal) {
        for year in years_in(answer) {
            if !blob.contains(&year) && year != chrono::Local::now().format("%Y").to_string() {
                out.push(format!("the year {year}"));
            }
        }
    }
    let said = format!("{} ", answer.to_lowercase());
    for group in CONDITION_GROUPS {
        let claimed = group.iter().find(|w| said.contains(**w));
        if let Some(word) = claimed {
            if !group.iter().any(|w| blob.contains(*w)) {
                out.push(word.trim().to_string());
            }
        }
    }
    out
}

/// Numbers in `text` that are precise enough to check against a page: they have a decimal part or
/// at least four digits. Thousands separators are removed ("72,529.07" -> "72529.07"). Years and
/// dates are left to `years_in`.
pub(super) fn precise_figures_in(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        // digits, with "," between digit groups and one "." before a fraction
        while i < chars.len() {
            let c = chars[i];
            let next_is_digit = chars.get(i + 1).is_some_and(|n| n.is_ascii_digit());
            if c.is_ascii_digit() || ((c == ',' || c == '.') && next_is_digit) {
                i += 1;
            } else {
                break;
            }
        }
        let raw: String = chars[start..i].iter().collect();
        let number = raw.replace(',', "");
        let has_fraction = number.contains('.');
        let digits = number.chars().filter(|c| c.is_ascii_digit()).count();
        let is_year = !has_fraction && number.len() == 4 && (number.starts_with("19") || number.starts_with("20"));
        if (has_fraction || digits >= 4) && !is_year && !out.contains(&number) {
            out.push(number);
        }
    }
    out
}

/// Four-digit years (1990-2099) written in `text`.
fn years_in(text: &str) -> Vec<String> {
    let bytes: Vec<char> = text.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let run: String = bytes[start..i].iter().collect();
            if run.len() == 4 && (run.starts_with("19") && run.as_str() >= "1990" || run.starts_with("20")) && !out.contains(&run) {
                out.push(run);
            }
        } else {
            i += 1;
        }
    }
    out
}

/// "The observations do not include…", "please provide…": a refusal dressed as an answer.
pub(super) fn final_says_data_is_missing(answer: &str) -> bool {
    let a = answer.to_ascii_lowercase();
    [
        "do not include",
        "does not include",
        "did not include",
        "not include specific",
        "no specific",
        "please provide",
        "please confirm",
        "provide additional",
        "unable to find",
        "unable to determine",
        "could not find",
        "couldn't find",
        "cannot find",
        "can't find",
        "not able to find",
        "no information",
        "not available in the observation",
        "no data",
        "can be checked",
        "by visiting",
        "you can visit",
        "you can check",
        "refer to the",
        "are reflected in",
    ]
    .iter()
    .any(|p| a.contains(p))
}

/// How many finals this run has already been sent back on, so a page that truly has nothing ends
/// with the honest "not found" answer instead of looping.
fn rejections_so_far(history: &[(String, String)]) -> usize {
    history.iter().filter(|(_, o)| o.contains("FINAL REJECTED")).count()
}

/// Did the last real tool result (not the loop's own notes that a final was sent back) report a failure?
pub(super) fn last_result_failed(history: &[(String, String)]) -> bool {
    history.iter().rev().find(|(_, o)| !o.contains("FINAL REJECTED")).is_some_and(|(_, o)| observation_indicates_failure(o))
}

/// How many finals this run has already been sent back on because the words of the goal were not found in anything it read.
/// That check is a heuristic (a misheard word or a word the page phrases differently fails it however good the answer is),
/// so it may send a final back once; after that the checks on what the answer claims decide.
pub(super) fn coverage_rejections_so_far(history: &[(String, String)]) -> usize {
    history.iter().filter(|(_, o)| o.contains("FINAL REJECTED") && o.contains("do not evidence goal entities")).count()
}

/// A coding task is not done because the model says so: send it back to change the code and run the
/// checks, up to five times. An answer that admits it could not do the task is let through (it will
/// be reported as unverified, not retried forever).
pub(super) fn reject_unchecked_coding_final(goal: &str, answer: &str, steps: &[AgentStep], history: &[(String, String)]) -> Option<String> {
    use crate::modules::code_verification::{run_is_coding, CodeEvidence};
    // A coding run is one that changed source code or ran a check, as well as one whose goal says so (or the caller said so, card OA4-01).
    let coding = crate::modules::run_profile::coding_latched(goal) || run_is_coding(&crate::modules::agent_experience::tool_calls_of(steps));
    if !coding || goal_asks_for_information(goal) || rejections_so_far(history) >= 5 {
        return None;
    }
    if crate::modules::agent_experience::answer_admits_failure(answer) {
        return None;
    }
    // The kind of check the goal asks for settles it (card OA4-04, WF-07): `cargo check` does not say the tests pass.
    match crate::modules::agent_experience::code_evidence_for_goal(steps, goal) {
        CodeEvidence::Passed(_) => None,
        CodeEvidence::WrongKind { wanted, got } => {
            let how = match wanted {
                crate::modules::code_verification::CheckKind::Tests => " (for example exe cargo, args [\"test\"], cwd the project folder)",
                _ => "",
            };
            Some(format!("the goal asks for {}, but only {} ran: run {} now with term.run{how} and read its exit code", wanted.asked_for(), got.ran(), wanted.ran()))
        }
        CodeEvidence::None => {
            let changed = crate::modules::agent_experience::tool_calls_of(steps).iter().any(|(t, _, _)| crate::modules::code_verification::is_code_change(t));
            Some(if changed {
                "you changed the code but have not run any test or build: call term.run now (for example exe cargo, args [\"test\"], cwd the project folder) and read its exit code".into()
            } else {
                "you have not changed any file yet and no test has run: call fs.edit now with the exact text to replace (\"old\") and its replacement (\"new\"), then run the tests with term.run".into()
            })
        }
        CodeEvidence::Failed(_) => Some("the last check failed: read its output, fix the code with fs.edit and run the check again".into()),
        CodeEvidence::Stale(_) => Some("the code changed after the last check: run the tests or build again".into()),
    }
}

pub(super) fn reject_premature_final(
    goal: &str,
    answer: &str,
    history: &[(String, String)],
) -> Option<String> {
    if is_placeholder_final(answer) {
        return Some(
            "that final is a placeholder, not an answer; write the real answer using the facts in the observations"
                .into(),
        );
    }
    // An honest "I could not do it", after a step that failed, is how such a run ends. Every check below asks for more work while the last
    // result is a failure, and the loop's own "FINAL REJECTED" note counts as one: so a truthful answer was sent back every round until the
    // steps or the 180 seconds ran out ("I ran out of time"), minutes after the person could have been told. The model is sent back once, to
    // try another way as the message says; the second time it says so, that is the answer.
    if crate::modules::agent_experience::answer_admits_failure(answer) && last_result_failed(history) && rejections_so_far(history) >= 1 {
        return None;
    }
    let unsupported = unsupported_claims(goal, answer, history);
    if !unsupported.is_empty() {
        return Some(format!(
            "the answer states {} but nothing you read shows it; read the page again (browser_snapshot) and              answer only with values that appear in it",
            unsupported.join(", ")
        ));
    }
    if goal_asks_for_information(goal) && final_says_data_is_missing(answer) && rejections_so_far(history) < 3 {
        return Some(
            "the answer says the data is missing, but pages usually hold it in result snippets and cards;              read the page again (browser_snapshot, or aqua_browser_extract) and answer with the figures you find.              Do not ask the user for more information"
                .into(),
        );
    }
    if goal_wants_figures(goal)
        && !answer.chars().any(|c| c.is_ascii_digit())
        && history.iter().any(|(_, o)| !key_figure_lines(&compact_page_observation(o), 1).is_empty())
        && rejections_so_far(history) < 3
    {
        return Some(
            "the user wants current figures and the page has them, but the answer has no numbers; \
             quote the actual values (index levels, percentages, prices) from the page"
                .into(),
        );
    }
    if goal_asks_for_information(goal) && answer.split_whitespace().count() < 3 {
        return Some("the user asked for information; the final answer must state it, not just confirm".into());
    }
    if final_claims_future_work(answer) {
        return Some(
            "final describes future work (\"I will now…\") instead of completed goal".into(),
        );
    }
    if final_only_claims_retrieval(answer) {
        return Some(
            "final only says the data was retrieved; put the requested information itself \
             (e.g. the list of headlines from the observation) in the final answer"
                .into(),
        );
    }
    if goal_has_follow_on_work(goal) && answer_only_claims_navigation(answer) {
        return Some(
            "goal requires follow-on actions beyond opening/navigating; continue acting (compose/send/click/type/etc.)"
                .into(),
        );
    }
    if history
        .last()
        .map(|(_, o)| observation_indicates_failure(o))
        .unwrap_or(false)
    {
        return Some(
            "last tool observation is a failure; try an alternative tool or report honest failure"
                .into(),
        );
    }
    if needs_observation_before_final(history) {
        return Some(
            "last tool was an action/ack only; observe UI/state (Snapshot/browser_snapshot) before final"
                .into(),
        );
    }
    if !observations_cover_goal(goal, history) && coverage_rejections_so_far(history) < 1 {
        let positive = positive_history(history);
        let missing: Vec<_> = hard_goal_tokens(goal)
            .into_iter()
            .filter(|t| !positive.iter().any(|(_, o)| evidence_has_word(&o.to_ascii_lowercase(), t)))
            .collect();
        return Some(format!(
            "observations do not evidence goal entities {:?}; keep acting/observing (failures do not count as evidence)",
            missing
        ));
    }
    if answer_claims_success(answer) && !observations_cover_goal(goal, history) {
        return Some(
            "success claim without positive verified evidence that the goal is actually open/done"
                .into(),
        );
    }
    // "Open X" goals: success wording requires entity in a positive observation.
    if answer_claims_success(answer) {
        let openish = {
            let g = goal.to_ascii_lowercase();
            g.contains("open ")
                || g.starts_with("open")
                || g.contains("launch ")
                || g.contains("start ")
                || g.contains("go to ")
                || g.contains("navigate")
        };
        if openish && !observations_cover_goal(goal, history) {
            return Some(
                "claimed open/success but no positive observation shows the target is open".into(),
            );
        }
    }
    if !last_result_is_verified_observation(history) && !history.is_empty() {
        return Some("no verified observation of current UI/state yet".into());
    }
    None
}

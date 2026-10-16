//! Research on a gap (card R7-08): when Aqua does not know how to do something, it can look the answer up and attach what it found to the gap, as text for the person to read.
//!
//! * It starts only from the person's click (`POST /api/v1/self-repair/gaps/:id/research`, the UI token). The watch never researches.
//! * It is off unless `AQUA_RESEARCH=on` (the tool `research.run` exists only then). With it off, [`research_gap`] returns [`Researched::Disabled`] with the reason and does nothing.
//! * The question is the gap's own summary and nothing else ([`question_for`]): not the request it came from, not the detail, not the facts of the gap, no address. The summary is
//!   written by Aqua, scrubbed of secrets and cut before it is stored.
//! * The research is one call of `research.run` through the tool host, as the app: the same decision, the same audit log, and the searches inside it are calls of their own. It is asked
//!   for depth 2 (searches and their dated snippets, no page is read), the lightest research there is.
//! * What comes back is **data for the person**. It is stored on the gap as `finding`, marked as from outside, with its text cut, and it is never read as an instruction and never turned
//!   into a proposal: `proposals::create_proposals_for` looks at the kind and the facts of a gap and at nothing in the finding.

use super::store::{clean_text, Gap, GapKind, RepairError, RepairStore, R};
use serde_json::{json, Value};

/// The depth asked of `research.run`: 2 is the tool's own lightest tier (searches only; its tier named T1 is a weather feed that searches nothing).
pub const DEPTH: u8 = 2;
/// The longest question, in characters.
pub const MAX_QUESTION_CHARS: usize = 300;
/// The longest answer kept in a finding, in characters.
pub const MAX_ANSWER_CHARS: usize = 1_500;
/// The most sources listed in a finding (the rest are counted).
pub const MAX_SOURCES: usize = 8;
const MAX_TITLE_CHARS: usize = 120;
const MAX_URL_CHARS: usize = 300;
const MAX_DATE_CHARS: usize = 40;
const MIN_QUESTION_WORDS: usize = 2;

/// Said in every finding, so that whatever shows it says whose words these are.
pub const NOTICE: &str = "This text came from the web. It is shown to you as information: Aqua does not follow it and makes no fix from it. Check it before you rely on it.";

/// How a research is run. The real one calls the tool host; a test gives a fake.
pub trait ResearchHost: Send + Sync {
    /// `Err` with the reason when research cannot run now (switched off, or the tool is not there).
    fn available(&self) -> Result<(), String>;
    /// Run `research.run` for the question at the depth: the data of the tool's answer, or why there was none.
    fn run(&self, question: &str, depth: u8) -> Result<Value, String>;
}

/// The running tool host, called as the app (the person's own click). The host decides and records the call like any other.
pub struct HostResearch;

impl ResearchHost for HostResearch {
    fn available(&self) -> Result<(), String> {
        if !crate::research::enabled() {
            return Err("research is switched off: it runs only when Aqua is started with AQUA_RESEARCH=on".to_string());
        }
        let host = crate::agent_tools::host().map(|h| h.as_ref());
        if !crate::agent_tools::is_host_tool(host, crate::research::tool::TOOL) {
            return Err("the tool host does not have the research tool (it is added when Aqua starts with AQUA_RESEARCH=on)".to_string());
        }
        Ok(())
    }

    fn run(&self, question: &str, depth: u8) -> Result<Value, String> {
        let host = crate::agent_tools::host().ok_or_else(|| "the tool host is not running, so nothing can be researched now".to_string())?;
        let context = aqua_tool_host::RequestContext::new(aqua_tool_host::Caller::Ui);
        let res = host.call_blocking(aqua_tool_host::ToolRequest { tool: crate::research::tool::TOOL.into(), arguments: json!({ "question": question, "depth": depth }), context });
        if res.ok {
            Ok(res.data)
        } else {
            let why = res.error.as_ref().map(|e| e.message.clone()).filter(|m| !m.is_empty()).unwrap_or_else(|| res.text.clone());
            Err(format!("the research did not run: {why}"))
        }
    }
}

/// What came of asking for a research.
#[derive(Debug, Clone, PartialEq)]
pub enum Researched {
    /// Research is off, or the tool is not there: nothing was done. The reason is for the person.
    Disabled(String),
    /// The research was tried and did not run (no search answered, the host said no): nothing was stored.
    Failed(String),
    /// The research ran. The gap now carries the finding, which may say that no source answers the question.
    Done(Gap),
}

/// The question to research for a gap: its summary, and nothing else. A word that is an address is dropped (the research reads an address that is in its question, and a gap is no place
/// to name one), hidden characters are removed, secrets are scrubbed, and the text is cut. `Err` when too little is left to ask.
pub fn question_for(gap: &Gap) -> Result<String, String> {
    let visible = aqua_tool_host::output::sanitize(&gap.summary);
    let words: Vec<&str> = visible.split_whitespace().filter(|w| !w.contains("://") && !w.to_ascii_lowercase().starts_with("www.")).collect();
    let question = clean_text(&words.join(" "), MAX_QUESTION_CHARS);
    if question.split_whitespace().count() < MIN_QUESTION_WORDS {
        return Err("the summary of this gap is too short to make a question from".to_string());
    }
    Ok(question)
}

/// A text from outside for a finding: hidden characters out, secrets scrubbed, cut.
fn outside_text(text: &str, max: usize) -> String {
    clean_text(&aqua_tool_host::output::sanitize(text), max)
}

fn source_line(s: &Value) -> Value {
    let url = s["url"].as_str().unwrap_or("").trim();
    let is_web = url.starts_with("https://") || url.starts_with("http://");
    let host = if is_web { url::Url::parse(url).ok().and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_lowercase())).unwrap_or_default() } else { String::new() };
    let date = s["date"].as_str().map(|d| outside_text(d, MAX_DATE_CHARS)).filter(|d| !d.is_empty());
    let shown_url = if is_web { outside_text(url, MAX_URL_CHARS) } else { String::new() };
    json!({
        "n": s["id"].as_u64().unwrap_or(0),
        "host": outside_text(&host, 80),
        "date": date,
        "title": outside_text(s["title"].as_str().unwrap_or(""), MAX_TITLE_CHARS),
        "url": shown_url,
    })
}

/// The finding for a research: what `research.run` gave back (`data`, the tool's own answer), cut and marked as from outside. It holds the answer with its numbered sentences, the
/// numbered sources with their host and date, and what was left out: sources past the cap, and sentences of the sources that gave an order and were not used.
pub fn finding_from(question: &str, data: &Value, now: i64) -> Value {
    let all: &[Value] = data["sources"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let sources: Vec<Value> = all.iter().take(MAX_SOURCES).map(source_line).collect();
    let left_out = data["left_out"].as_u64().unwrap_or(0);
    let left_out_text = if left_out == 0 {
        String::new()
    } else {
        format!(
            "{left_out} sentence{} in the sources gave an order or spoke to the reader. {} left out, and nothing was done about {}.",
            if left_out == 1 { "" } else { "s" },
            if left_out == 1 { "It was" } else { "They were" },
            if left_out == 1 { "it" } else { "them" }
        )
    };
    let mut finding = json!({
        "from_outside": true,
        "notice": NOTICE,
        "question": question,
        "at": now,
        "tier": outside_text(data["tier"].as_str().unwrap_or(""), 8),
        "answered": data["answered"].as_bool().unwrap_or(false),
        "answer": outside_text(data["answer"].as_str().unwrap_or(""), MAX_ANSWER_CHARS),
        "sources": sources,
        "sources_not_shown": all.len().saturating_sub(MAX_SOURCES),
        "contradictions": data["contradictions"].as_u64().unwrap_or(0),
        "searches": data["queries"].as_array().map_or(0, Vec::len),
        "left_out": left_out,
        "left_out_text": left_out_text,
    });
    if let Some(report) = data["report"].as_str().filter(|r| !r.trim().is_empty()) {
        finding["report"] = json!(outside_text(report, MAX_URL_CHARS));
    }
    finding
}

/// Look up a gap that says Aqua does not know how, and attach the cited finding to it.
///
/// `Err(NotFound)` for no such gap; `Err(Refused)` for a gap that is dismissed, of another kind, or whose summary is too short to ask. `Ok(Disabled)` when research cannot run. Nothing
/// else about the gap changes: its status, its proposals and its count stay as they were.
pub fn research_gap(store: &RepairStore, host: &dyn ResearchHost, gap_id: i64, now: i64) -> R<Researched> {
    let gap = store.gap(gap_id)?.ok_or_else(|| RepairError::NotFound("no such gap".into()))?;
    if let Err(why) = host.available() {
        return Ok(Researched::Disabled(why));
    }
    if gap.status == "dismissed" {
        return Err(RepairError::Refused("this gap was dismissed; nothing is looked up for it".into()));
    }
    if gap.kind != GapKind::HowTo.as_str() {
        return Err(RepairError::Refused(format!("research is for a gap that says Aqua does not know how to do something (kind how_to); this gap is of the kind {}", gap.kind)));
    }
    let question = question_for(&gap).map_err(RepairError::Refused)?;
    let data = match host.run(&question, DEPTH) {
        Ok(data) => data,
        Err(why) => return Ok(Researched::Failed(clean_text(&why, 300))),
    };
    store.set_finding(gap_id, &finding_from(&question, &data, now))?;
    let gap = store.gap(gap_id)?.ok_or_else(|| RepairError::NotFound("no such gap".into()))?;
    Ok(Researched::Done(gap))
}

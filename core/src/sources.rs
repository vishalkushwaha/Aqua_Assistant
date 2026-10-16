//! What a run read on the web, and whether its answer says only what that text says.
//!
//! Every search result and every page the agent reads is entered here with a number (the `[n]` the search list showed), its address, title and date,
//! and the words it gave: the snippet of the search, the text of the page. The answer is then checked by the program, not by the model: each sentence
//! that cites `[n]` must be about what source n said (its numbers and its names must be in that text), and each sentence that cites nothing but states a
//! number or a name is either found in a source (and cited) or marked. A citation that does not exist, or a claim the cited text does not make, is
//! marked in the answer; it is never left looking verified.
//!
//! Only what the run itself fetched can be a source, and only that text can verify a claim: the model's memory cannot.

use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// One thing the run read.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Source {
    pub id: usize,
    pub url: String,
    pub title: String,
    pub date: Option<String>,
    /// The words this source gave (snippet of a search result, then the text of the page when it was read). Not part of the answer.
    #[serde(skip)]
    pub spans: Vec<String>,
}

#[derive(Default)]
struct Run {
    sources: Vec<Source>,
    /// Addresses the person gave in the request: a page at one of these may be read without a search having named it.
    goal_urls: Vec<String>,
    /// What the person asked (the reader is told what the page is read for).
    goal: String,
}

fn runs() -> std::sync::MutexGuard<'static, HashMap<String, Run>> {
    static RUNS: OnceLock<Mutex<HashMap<String, Run>>> = OnceLock::new();
    RUNS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap_or_else(|e| e.into_inner())
}

/// The addresses in a piece of text (what the person typed).
pub fn urls_in(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|t| {
            // What is written around an address (a bracket, a quote, a stop or a comma after it) is taken off until nothing more comes off: `(see https://a.com/x).` ends in `).`, a
            // stop after a bracket, and one pass of each kind left the bracket on the address (found by `core/tests/text_properties.rs`).
            let mut t = t;
            loop {
                let shorter = t.trim_matches(|c: char| matches!(c, ',' | ';' | '"' | '\'' | '(' | ')' | '<' | '>' | '`')).trim_end_matches(['.', '!', '?']);
                if shorter.len() == t.len() {
                    break;
                }
                t = shorter;
            }
            (t.starts_with("https://") || t.starts_with("http://")).then(|| t.to_string())
        })
        .collect()
}

/// Same page, whatever the fragment, a trailing slash or the case of the host.
pub fn url_key(url: &str) -> String {
    url.split('#').next().unwrap_or("").trim_end_matches('/').to_lowercase()
}

/// Starts the record of a run (the goal's own addresses are the ones it may read without a search naming them).
pub fn begin(run_id: &str, goal: &str) {
    runs().insert(run_id.to_string(), Run { sources: Vec::new(), goal_urls: urls_in(goal), goal: goal.to_string() });
}

/// [`begin`] for a helper agent that a run started (card OA11-04): its question was written by a model, not given by the person, so **no address in it is one the person gave**. It may read a
/// page only if a search of its own named it; an address that the model of the run composed (with something it knows in its query) must not become readable by being put in a question.
pub fn begin_for_helper(run_id: &str, question: &str) {
    runs().insert(run_id.to_string(), Run { sources: Vec::new(), goal_urls: Vec::new(), goal: question.to_string() });
}

/// What the run was asked to do.
pub fn goal_of(run_id: &str) -> String {
    runs().get(run_id).map(|r| r.goal.clone()).unwrap_or_default()
}

/// The run is over: its sources are forgotten.
pub fn end(run_id: &str) {
    runs().remove(run_id);
}

pub fn count(run_id: &str) -> usize {
    runs().get(run_id).map_or(0, |r| r.sources.len())
}

/// Is this run being recorded (`begin` was called for it and `end` was not)? A tool that adds sources of its own (card R2-10, `research.run`) does so only for such a run: for any
/// other, `add_hit` would make a record that nothing ends.
pub fn is_tracking(run_id: &str) -> bool {
    runs().contains_key(run_id)
}

pub fn sources_of(run_id: &str) -> Vec<Source> {
    runs().get(run_id).map(|r| r.sources.clone()).unwrap_or_default()
}

/// May this run read `url`? Only an address the person gave, or one a search result of this run named. A web address a model made up (with
/// somebody's data in its query, say) is not one of those.
pub fn may_read(run_id: &str, url: &str) -> bool {
    let key = url_key(url);
    runs().get(run_id).is_some_and(|r| r.goal_urls.iter().any(|g| url_key(g) == key) || r.sources.iter().any(|s| url_key(&s.url) == key))
}

/// Did a search of this run (or a page it read) name `url`? Not an address the goal contains: what the person typed is theirs to read, but an address in a goal that was
/// spoken may be what a television said, so for a spoken request only a result of the run's own search is a page that was named to it.
pub fn named_by_a_search(run_id: &str, url: &str) -> bool {
    let key = url_key(url);
    runs().get(run_id).is_some_and(|r| r.sources.iter().any(|s| url_key(&s.url) == key))
}

/// Enters a search result; returns its number. The same page twice is one source.
pub fn add_hit(run_id: &str, url: &str, title: &str, date: Option<&str>, snippet: &str) -> usize {
    let mut all = runs();
    let run = all.entry(run_id.to_string()).or_default();
    let key = url_key(url);
    if let Some(s) = run.sources.iter_mut().find(|s| url_key(&s.url) == key) {
        if !snippet.trim().is_empty() && !s.spans.iter().any(|x| x == snippet) {
            s.spans.push(snippet.to_string());
        }
        return s.id;
    }
    let id = run.sources.len() + 1;
    run.sources.push(Source { id, url: url.to_string(), title: title.to_string(), date: date.map(str::to_string), spans: if snippet.trim().is_empty() { vec![] } else { vec![snippet.to_string()] } });
    id
}

/// Enters the text of a page the run read.
pub fn add_page(run_id: &str, url: &str, title: &str, date: Option<&str>, text: &str) -> usize {
    let id = add_hit(run_id, url, title, date, "");
    let mut all = runs();
    if let Some(s) = all.get_mut(run_id).and_then(|r| r.sources.iter_mut().find(|s| s.id == id)) {
        if s.title.is_empty() {
            s.title = title.to_string();
        }
        if s.date.is_none() {
            s.date = date.map(str::to_string);
        }
        s.spans.push(text.chars().take(20_000).collect());
    }
    id
}

// ---- checking an answer -----------------------------------------------------------------------------------------------------------

/// A sentence without its citation markers (`[1]`, `[2, 3]`, `[?]`), which are not part of the claim.
fn without_citation_markers(sentence: &str) -> String {
    let mut out = String::new();
    let mut rest = sentence;
    while let Some(i) = rest.find('[') {
        out.push_str(&rest[..i]);
        match rest[i..].find(']') {
            Some(j) if rest[i + 1..i + j].chars().all(|c| c.is_ascii_digit() || c == ',' || c == ' ' || c == '?') => rest = &rest[i + j + 1..],
            _ => {
                out.push('[');
                rest = &rest[i + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The numbers in a text, as digits without thousands separators (`1,250,000` is `1250000`; a decimal point is kept when a digit follows it), with where each begins and ends in the
/// text (byte offsets).
fn number_spans(clean: &str) -> Vec<(String, usize, usize)> {
    let chars: Vec<(usize, char)> = clean.char_indices().collect();
    let mut out: Vec<(String, usize, usize)> = Vec::new();
    let (mut digits, mut start) = (String::new(), 0usize);
    for (k, &(at, c)) in chars.iter().enumerate() {
        let digit_follows = chars.get(k + 1).is_some_and(|(_, n)| n.is_ascii_digit());
        let keep = c.is_ascii_digit() || ((c == '.' || c == ',') && !digits.is_empty() && digit_follows);
        if keep {
            if digits.is_empty() {
                start = at;
            }
            if c != ',' {
                digits.push(c);
            }
        } else if !digits.is_empty() {
            out.push((std::mem::take(&mut digits), start, at));
        }
    }
    if !digits.is_empty() {
        out.push((digits, start, clean.len()));
    }
    out
}

/// The pieces of a text a claim can be checked against: numbers (without thousands separators), and capitalised words that are not the first
/// word of the sentence.
fn checkable(sentence: &str) -> (Vec<String>, Vec<String>) {
    let clean = without_citation_markers(sentence);
    let numbers: Vec<String> = number_spans(&clean).into_iter().map(|(n, _, _)| n).collect();
    let mut names: Vec<String> = Vec::new();
    let words: Vec<&str> = clean.split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '-')).filter(|w| !w.is_empty()).collect();
    for (i, w) in words.iter().enumerate() {
        let w = w.trim_matches(|c: char| c == '\'' || c == '-');
        let capital = w.chars().next().is_some_and(|c| c.is_uppercase()) && w.chars().count() >= 3 && !w.chars().all(|c| c.is_uppercase() && c.is_alphabetic() && w.chars().count() <= 2);
        if capital && i > 0 && !["The", "This", "That", "These", "Those", "There", "However", "According", "Also", "But", "And", "It", "He", "She", "They", "We", "In", "On", "At", "For", "As", "If", "When", "While", "Some", "Many", "Most"].contains(&w) {
            let lower = w.to_lowercase();
            if !names.contains(&lower) {
                names.push(lower);
            }
        }
    }
    (numbers, names)
}

fn span_text(spans: &[&Source]) -> String {
    spans.iter().flat_map(|s| s.spans.iter()).map(|t| t.to_lowercase().replace(',', "")).collect::<Vec<_>>().join("\n")
}

/// Is `number` (digits, maybe with a decimal point) in `hay` as a number of its own? "4" is not in "14 million", "2" is not in "2024" and "3" is not in "3.5": a number
/// counts only where no digit and no decimal fraction runs on from either side. (It used to be found anywhere in the text, so a claim of 4 million was found in a source that
/// said 14 million.)
#[cfg(test)]
fn has_number(hay: &str, number: &str) -> bool {
    !number_places(hay, number).is_empty()
}

/// Where `number` stands in `hay` as a number of its own (see [`has_number`]): the start and end of each place, as byte offsets.
fn number_places(hay: &str, number: &str) -> Vec<(usize, usize)> {
    let mut places: Vec<(usize, usize)> = Vec::new();
    let bytes = hay.as_bytes();
    let mut from = 0;
    while let Some(at) = hay[from..].find(number) {
        let start = from + at;
        let end = start + number.len();
        let before_ok = match start.checked_sub(1).map(|i| bytes[i]) {
            None => true,
            Some(b) if b.is_ascii_digit() => false,
            Some(b'.') => !(start >= 2 && bytes[start - 2].is_ascii_digit()),
            Some(_) => true,
        };
        let after_ok = match bytes.get(end) {
            None => true,
            Some(b) if b.is_ascii_digit() => false,
            Some(b'.') => !bytes.get(end + 1).is_some_and(u8::is_ascii_digit),
            Some(_) => true,
        };
        if before_ok && after_ok {
            places.push((start, end));
        }
        from = start + 1;
        while from < hay.len() && !hay.is_char_boundary(from) {
            from += 1;
        }
    }
    places
}

/// The unit that goes with a number: the word or sign after it (`14 million`, `5%`, `20°C`, `3 km`, `2 days`), or the currency sign before it (`$5`). `None` when it has no unit of a kind
/// that is known. What matters is that two units of the same family are told apart (million and billion, Celsius and Fahrenheit, a percentage and an amount).
fn unit_of(text: &str, start: usize, end: usize) -> Option<&'static str> {
    let rest = &text[end..];
    // The sign or word after the number, past a space, a no-break space or a hyphen (`5-year`).
    let rest = rest.trim_start_matches([' ', '\u{a0}', '-']);
    if rest.starts_with('%') {
        return Some("percent");
    }
    let (rest, degrees) = match rest.strip_prefix('°') {
        Some(after) => (after.trim_start_matches([' ', '\u{a0}']), true),
        None => (rest, false),
    };
    let word: String = rest.chars().take_while(|c| c.is_alphabetic()).collect::<String>().to_lowercase();
    if degrees {
        return match word.as_str() {
            "c" | "celsius" | "centigrade" => Some("celsius"),
            "f" | "fahrenheit" => Some("fahrenheit"),
            _ => None,
        };
    }
    let by_word = match word.as_str() {
        "percent" | "pct" => Some("percent"),
        "thousand" => Some("thousand"),
        "million" | "mn" => Some("million"),
        "billion" | "bn" => Some("billion"),
        "trillion" | "tn" => Some("trillion"),
        "celsius" | "centigrade" => Some("celsius"),
        "fahrenheit" => Some("fahrenheit"),
        "degrees" | "degree" => {
            // `20 degrees Celsius`: the word after "degrees" says which.
            let after = rest[word.len()..].trim_start();
            let next: String = after.chars().take_while(|c| c.is_alphabetic()).collect::<String>().to_lowercase();
            match next.as_str() {
                "celsius" | "centigrade" => Some("celsius"),
                "fahrenheit" => Some("fahrenheit"),
                _ => None,
            }
        }
        "km" | "kilometer" | "kilometers" | "kilometre" | "kilometres" => Some("km"),
        "mile" | "miles" | "mi" => Some("mile"),
        "kg" | "kilogram" | "kilograms" => Some("kg"),
        "lb" | "lbs" => Some("lb"),
        "kb" | "kilobyte" | "kilobytes" => Some("kb"),
        "mb" | "megabyte" | "megabytes" => Some("mb"),
        "gb" | "gigabyte" | "gigabytes" => Some("gb"),
        "tb" | "terabyte" | "terabytes" => Some("tb"),
        "dollar" | "dollars" | "usd" => Some("usd"),
        "euro" | "euros" | "eur" => Some("eur"),
        "rupee" | "rupees" | "inr" => Some("inr"),
        "second" | "seconds" => Some("second"),
        "minute" | "minutes" => Some("minute"),
        "hour" | "hours" => Some("hour"),
        "day" | "days" => Some("day"),
        "week" | "weeks" => Some("week"),
        "month" | "months" => Some("month"),
        "year" | "years" => Some("year"),
        _ => None,
    };
    if by_word.is_some() {
        return by_word;
    }
    // A currency sign in front (`$5`, `€5`) when nothing after the number says its unit.
    match text[..start].chars().next_back() {
        Some('$') => Some("usd"),
        Some('€') => Some("eur"),
        Some('£') => Some("gbp"),
        Some('₹') => Some("inr"),
        _ => None,
    }
}

/// How many zeros a scale word stands for: `Some(0)` for no unit at all, `Some(6)` for million, `None` for a unit that is not a scale (a percentage, a distance).
fn scale_zeros(unit: Option<&str>) -> Option<usize> {
    match unit {
        None => Some(0),
        Some("thousand") => Some(3),
        Some("million") => Some(6),
        Some("billion") => Some(9),
        Some("trillion") => Some(12),
        Some(_) => None,
    }
}

/// `number` and its scale written out whole, in digits: `2.5` million is `2500000` and `25.2` billion is `25200000000`, so that the same quantity written in two ways is the same
/// string. `None` for a unit that is not a scale, and for a number that does not come out whole.
fn value_of(number: &str, unit: Option<&str>) -> Option<String> {
    let zeros = scale_zeros(unit)?;
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if fraction.len() > zeros || !whole.chars().chain(fraction.chars()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    let digits = format!("{whole}{fraction}{}", "0".repeat(zeros - fraction.len()));
    let trimmed = digits.trim_start_matches('0');
    Some(if trimmed.is_empty() { "0".to_string() } else { trimmed.to_string() })
}

/// Is `number` in `hay` as a number of its own, and with a unit that does not contradict the claim's (card R6-18)? A claim with no unit is met by any place the number stands. A claim
/// with one (`5 million`, `20°C`, `5%`) is met where the number has the same unit, or has no unit of a kind that is known (`5 apples`). It is not met when every place the number
/// stands has another known unit (`5 billion`, `20°F`, `5 km` for a claim of `5 miles`). Either way it is also met by the same quantity written another way (`2.5 million` and
/// `2500000`, `25,200 million` and `25.2 billion`): only for scales, where the two are the same number.
fn has_number_with_unit(hay: &str, number: &str, unit: Option<&'static str>) -> bool {
    let places = number_places(hay, number);
    let met_where_it_stands = match unit {
        None => !places.is_empty(),
        Some(wanted) => places.iter().any(|&(start, end)| unit_of(hay, start, end).map_or(true, |found| found == wanted)),
    };
    if met_where_it_stands {
        return true;
    }
    let Some(want) = value_of(number, unit) else { return false };
    // At the source's side only a scale word changes the quantity (`2500000 dollars` is 2500000, `25.2 billion` is 25200000000); any other unit is left out of it.
    number_spans(hay).iter().any(|(n, start, end)| value_of(n, unit_of(hay, *start, *end).filter(|u| scale_zeros(Some(*u)).is_some())).is_some_and(|written| written == want))
}

/// How much of a claim's names the cited text must hold, in percent. It is the one threshold of the check, and it is reported with every answer that was checked (the log line,
/// the metric `aqua_verifier_name_share_percent`) next to the rate of claims the sources did not support, so that a rate is read with the setting that made it. It is set by the
/// measurement in `core/tests/verifier_calibration_test.rs` against `core/tests/fixtures/verifier_cases.json` (see the progress document, card R6-30).
pub const NAME_SHARE_PERCENT: usize = 70;

/// Does `text` (what the cited sources said) make this claim? Every number in the claim must be in it, and at least `name_share_percent` of its names.
pub fn verify_claim_with(claim: &str, text: &str, name_share_percent: usize) -> bool {
    let hay = text.to_lowercase().replace(',', "");
    let (_, names) = checkable(claim);
    // Every number of the claim must be in the text, and with the unit the claim gives it (`5 million` is not `5 billion`, `20°C` is not `20°F`, `5%` is not an amount of 5).
    let clean = without_citation_markers(claim);
    if !number_spans(&clean).iter().all(|(n, start, end)| has_number_with_unit(&hay, n, unit_of(&clean, *start, *end))) {
        return false;
    }
    if names.is_empty() {
        return true;
    }
    let found = names.iter().filter(|n| hay.contains(n.as_str())).count();
    found * 100 >= names.len() * name_share_percent
}

/// [`verify_claim_with`] at the threshold in force ([`NAME_SHARE_PERCENT`]).
pub fn verify_claim(claim: &str, text: &str) -> bool {
    verify_claim_with(claim, text, NAME_SHARE_PERCENT)
}

/// A sentence, with the numbers of the sources it cites.
#[derive(Debug, Clone, PartialEq)]
struct Sentence {
    text: String,
    cites: Vec<usize>,
}

fn cites_in(sentence: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut rest = sentence;
    while let Some(i) = rest.find('[') {
        let Some(j) = rest[i..].find(']') else { break };
        for part in rest[i + 1..i + j].split(',') {
            if let Ok(n) = part.trim().parse::<usize>() {
                if !out.contains(&n) {
                    out.push(n);
                }
            }
        }
        rest = &rest[i + j + 1..];
    }
    out
}

/// Cuts text into sentences, keeping what ends each one (a citation after the full stop stays with its sentence).
fn sentences(answer: &str) -> Vec<Sentence> {
    let mut out: Vec<String> = Vec::new();
    for line in answer.split('\n') {
        let mut cur = String::new();
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            cur.push(chars[i]);
            let ends = matches!(chars[i], '.' | '!' | '?') && chars.get(i + 1).is_none_or(|n| n.is_whitespace() || *n == '[');
            // A number like 3.5 or an abbreviation like "e.g." does not end a sentence.
            let decimal = chars[i] == '.' && i > 0 && chars[i - 1].is_ascii_digit() && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit());
            if ends && !decimal {
                // Citations written after the full stop belong to this sentence.
                while i + 1 < chars.len() && (chars[i + 1] == ' ' && chars.get(i + 2) == Some(&'[') || chars[i + 1] == '[') {
                    i += 1;
                    cur.push(chars[i]);
                    if chars[i] == '[' {
                        while i + 1 < chars.len() && chars[i] != ']' {
                            i += 1;
                            cur.push(chars[i]);
                        }
                    }
                }
                out.push(std::mem::take(&mut cur));
            }
            i += 1;
        }
        if !cur.trim().is_empty() {
            out.push(cur);
        }
    }
    out.into_iter().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).map(|t| Sentence { cites: cites_in(&t), text: t }).collect()
}

/// The result of checking an answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Checked {
    /// The answer, with each claim that could not be checked marked, citations added where a source was found, and the list of what was cited.
    pub text: String,
    /// The sources the final text cites, in order of number.
    pub cited: Vec<usize>,
    /// How many claims the cited sources did not support, and how many had no source at all.
    pub unsupported: usize,
    pub unsourced: usize,
    /// How many claims a source was found to support (cited by the answer, or found for it).
    pub supported: usize,
    /// The threshold the check was made with ([`NAME_SHARE_PERCENT`]).
    pub name_share_percent: usize,
}

impl Checked {
    /// The share of the claims that were checked against a source that the source did not support (0 to 1), or `None` when none was checked. Read it with `name_share_percent`.
    pub fn unsupported_rate(&self) -> Option<f64> {
        let checked = self.supported + self.unsupported;
        (checked > 0).then(|| self.unsupported as f64 / checked as f64)
    }

    /// One line for the log and for whoever reads a report: the counts, the rate and the threshold they were made with.
    pub fn summary(&self) -> String {
        format!(
            "answer check: {} supported, {} not supported by the cited source, {} with no source; unsupported-citation rate {}; name threshold {}%",
            self.supported,
            self.unsupported,
            self.unsourced,
            self.unsupported_rate().map(|r| format!("{:.0}%", r * 100.0)).unwrap_or_else(|| "n/a".into()),
            self.name_share_percent
        )
    }
}

/// Checks `answer` against what `run_id` read. With no sources, the answer is returned as it is.
pub fn check_answer(run_id: &str, answer: &str) -> Checked {
    let sources = sources_of(run_id);
    if sources.is_empty() {
        return Checked { text: answer.to_string(), cited: vec![], unsupported: 0, unsourced: 0, supported: 0, name_share_percent: NAME_SHARE_PERCENT };
    }
    let mut out_lines: Vec<String> = Vec::new();
    let (mut unsupported, mut unsourced, mut supported) = (0usize, 0usize, 0usize);
    let mut cited: Vec<usize> = Vec::new();
    // Rebuild line by line so lists and paragraphs keep their shape.
    for line in answer.split('\n') {
        let mut rebuilt: Vec<String> = Vec::new();
        for s in sentences(line) {
            let (numbers, names) = checkable(&s.text);
            let has_claim = !numbers.is_empty() || !names.is_empty();
            if !s.cites.is_empty() {
                let real: Vec<&Source> = s.cites.iter().filter_map(|n| sources.iter().find(|x| x.id == *n)).collect();
                if real.is_empty() {
                    unsupported += 1;
                    rebuilt.push(format!("{} (that source does not exist)", strip_cites(&s.text)));
                    continue;
                }
                let text = span_text(&real);
                if has_claim && !verify_claim(&s.text, &text) {
                    unsupported += 1;
                    rebuilt.push(format!("{} (the cited source does not say this)", s.text));
                } else {
                    if has_claim {
                        supported += 1;
                    }
                    for r in &real {
                        if !cited.contains(&r.id) {
                            cited.push(r.id);
                        }
                    }
                    rebuilt.push(s.text);
                }
            } else if has_claim && (!numbers.is_empty() || names.len() >= 2) {
                // A claim with no citation: found in one of the sources, it is cited now; found in none, it is marked.
                match sources.iter().find(|x| verify_claim(&s.text, &span_text(&[x]))) {
                    Some(src) => {
                        supported += 1;
                        if !cited.contains(&src.id) {
                            cited.push(src.id);
                        }
                        let end = s.text.trim_end();
                        let (body, stop) = match end.chars().last() {
                            Some(c @ ('.' | '!' | '?')) => (&end[..end.len() - c.len_utf8()], c.to_string()),
                            _ => (end, String::new()),
                        };
                        rebuilt.push(format!("{body} [{}]{stop}", src.id));
                    }
                    None => {
                        unsourced += 1;
                        rebuilt.push(format!("{} (no source says this)", s.text));
                    }
                }
            } else {
                rebuilt.push(s.text);
            }
        }
        out_lines.push(rebuilt.join(" "));
    }
    cited.sort_unstable();
    Checked { text: out_lines.join("\n"), cited, unsupported, unsourced, supported, name_share_percent: NAME_SHARE_PERCENT }
}

fn strip_cites(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find('[') {
        out.push_str(&rest[..i]);
        match rest[i..].find(']') {
            Some(j) => rest = &rest[i + j + 1..],
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The list the answer ends with: the sources it cites, as `[n] domain, date: title` and the address.
pub fn sources_block(run_id: &str, cited: &[usize]) -> String {
    let all = sources_of(run_id);
    let lines: Vec<String> = cited
        .iter()
        .filter_map(|id| all.iter().find(|s| s.id == *id))
        .map(|s| {
            let domain = url::Url::parse(&s.url).ok().and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_string())).unwrap_or_default();
            let when = s.date.as_deref().map(|d| format!(", {d}")).unwrap_or_default();
            let title = if s.title.is_empty() { String::new() } else { format!(": {}", s.title) };
            format!("[{}] {domain}{when}{title}\n    {}", s.id, s.url)
        })
        .collect();
    if lines.is_empty() {
        String::new()
    } else {
        format!("\n\nSources:\n{}", lines.join("\n"))
    }
}

/// A date a source gave, in the forms the search services use (`2025-09-14`, `2025-09-14T00:00:00Z`, `Mon, 01 Sep 2025`, `14 March 2025`, `March 14, 2025`). `None` for anything else,
/// such as "3 days ago": it is not a date this can count from.
pub(crate) fn parse_source_date(text: &str) -> Option<chrono::NaiveDate> {
    use chrono::NaiveDate;
    let t = text.trim();
    if let Some(d) = t.get(..10).and_then(|head| NaiveDate::parse_from_str(head, "%Y-%m-%d").ok()) {
        return Some(d);
    }
    // A weekday in front (`Mon, 01 Sep 2025 10:00:00 GMT`) and a time after the date are left out; the date is its first three words.
    let without_weekday = match t.split_once(", ") {
        Some((day, rest)) if day.len() <= 9 && day.chars().all(char::is_alphabetic) => rest,
        _ => t,
    };
    let words: Vec<&str> = without_weekday.split_whitespace().collect();
    if words.len() < 3 {
        return None;
    }
    let head = words[..3].join(" ");
    ["%d %B %Y", "%d %b %Y", "%B %d, %Y", "%b %d, %Y", "%B %d %Y", "%b %d %Y"].iter().find_map(|f| NaiveDate::parse_from_str(&head, f).ok())
}

/// How long before `today` a date was, in words for a person ("3 weeks ago").
fn age_in_words(days: i64) -> String {
    match days {
        d if d <= 0 => "today".to_string(),
        1 => "yesterday".to_string(),
        2..=13 => format!("{days} days ago"),
        14..=59 => format!("{} weeks ago", days / 7),
        60..=729 => format!("{} months ago", days / 30),
        _ => format!("{} years ago", days / 365),
    }
}

/// How old the newest of the cited sources is, in a sentence of its own that goes between the answer and the list of sources, so that an answer built on a page of last year does not
/// read (or sound) as if it were today's (card R6-18). Nothing when no source is cited; a sentence that says no source gave a date when none did.
pub fn data_age_sentence(run_id: &str, cited: &[usize]) -> String {
    data_age_sentence_at(&sources_of(run_id), cited, chrono::Local::now().date_naive())
}

pub(crate) fn data_age_sentence_at(all: &[Source], cited: &[usize], today: chrono::NaiveDate) -> String {
    let used: Vec<&Source> = all.iter().filter(|s| cited.contains(&s.id)).collect();
    if used.is_empty() {
        return String::new();
    }
    // No `[n]` in it: the sentence is spoken too (the list of sources is not), and a number between brackets is dropped from speech.
    let newest = used.iter().filter_map(|s| s.date.as_deref().and_then(parse_source_date)).max();
    match newest {
        None => "No source I read gave a date, so how old this information is cannot be said.".to_string(),
        Some(date) if date > today => format!("The newest source I read is dated {}, a date that has not come yet, so its age cannot be said.", date.format("%-d %B %Y")),
        Some(date) => format!("The newest source I read is dated {} ({}).", date.format("%-d %B %Y"), age_in_words((today - date).num_days())),
    }
}

/// The sources as data for a client.
pub fn sources_json(run_id: &str, cited: &[usize]) -> Vec<Value> {
    sources_of(run_id).into_iter().filter(|s| cited.contains(&s.id)).map(|s| serde_json::to_value(&s).unwrap_or(Value::Null)).collect()
}

/// A quantity a text states together with a unit of a known kind (card R2-10, the cross-check of research). `family` groups the units that can be compared: every scale word and
/// currency is "amount", a percentage is "percent", each other unit is its own. `value` writes the quantity the same way whatever the notation: `2.5 million`, `2,500,000 dollars` and
/// `2500000 dollars` give `2500000`, and `5 million` and `5 billion` do not give the same value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Quantity {
    pub family: &'static str,
    pub value: String,
}

/// A number as written, without a decimal tail of zeros (`5.0` is `5`; `1.90.0`, a version, is `1.90`).
fn plain_number(number: &str) -> String {
    match number.matches('.').count() {
        0 => number.to_string(),
        1 => number.trim_end_matches('0').trim_end_matches('.').to_string(),
        _ => {
            let mut n = number;
            while let Some(shorter) = n.strip_suffix(".0") {
                n = shorter;
            }
            n.to_string()
        }
    }
}

/// The numbers of a text: first those that carry a unit of a known kind, then the others as written (`1.90.0`, `14`, `2024`), which say what the sentence is about and not how much.
pub(crate) fn numbers_in(text: &str) -> (Vec<Quantity>, Vec<String>) {
    let clean = without_citation_markers(text);
    let (mut with_unit, mut bare) = (Vec::new(), Vec::new());
    for (number, start, end) in number_spans(&clean) {
        match unit_of(&clean, start, end) {
            Some(unit) => {
                let family = match unit {
                    "thousand" | "million" | "billion" | "trillion" | "usd" | "eur" | "inr" | "gbp" => "amount",
                    other => other,
                };
                let value = value_of(&number, Some(unit)).unwrap_or_else(|| plain_number(&number));
                with_unit.push(Quantity { family, value });
            }
            None => bare.push(plain_number(&number)),
        }
    }
    (with_unit, bare)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(name: &str) -> String {
        let id = format!("run_sources_test_{name}");
        begin(&id, "what is the latest rust release");
        add_hit(&id, "https://blog.rust-lang.org/2025/09/14/Rust-1.90.0.html", "Announcing Rust 1.90.0", Some("2025-09-14"), "The Rust team announced Rust 1.90.0 with lld as the default linker on x86_64 Linux.");
        add_hit(&id, "https://en.wikipedia.org/wiki/Rust_(programming_language)", "Rust (programming language)", Some("2025-08-30"), "Rust is a general-purpose programming language first released in 2015 by Graydon Hoare at Mozilla.");
        id
    }

    #[test]
    fn a_page_is_one_source_however_often_it_is_named_and_numbers_follow_the_order_they_came() {
        let id = run("numbering");
        assert_eq!(add_hit(&id, "https://blog.rust-lang.org/2025/09/14/Rust-1.90.0.html#top", "x", None, "more words"), 1, "the same page, a fragment later");
        assert_eq!(add_hit(&id, "https://example.com/new", "New", None, ""), 3);
        assert_eq!(count(&id), 3);
        assert_eq!(add_page(&id, "https://example.com/new", "New page", Some("2025-01-01"), "Full text of the page about Rust 1.90."), 3);
        let s = sources_of(&id);
        assert_eq!((s[2].title.as_str(), s[2].date.as_deref(), s[2].spans.len()), ("New", Some("2025-01-01"), 1), "the first title stays, a date the search did not have is taken from the page, and the text is added to what it said");
        end(&id);
        assert_eq!(count(&id), 0);
    }

    /// Card OA11-04 (security review): the question of a helper agent was written by a model, so an address in it is not one the person gave: the helper may read a page only when a search
    /// of its own named it. (Otherwise the model of a run that read something hostile could make any address readable, with what it knows in its query, by putting it in a question.)
    #[test]
    fn an_address_in_the_question_of_a_helper_is_not_one_the_person_gave() {
        let (own, helper) = ("run_sources_test_person_goal", "run_sources_test_helper_goal");
        begin(own, "read https://example.com/a for me");
        begin_for_helper(helper, "read https://example.com/a?secret=1234 for me");
        assert!(may_read(own, "https://example.com/a"), "the address the person gave is theirs to read");
        assert!(!may_read(helper, "https://example.com/a?secret=1234") && !may_read(helper, "https://example.com/a"), "a question that holds an address does not make it readable");
        assert_eq!(goal_of(helper), "read https://example.com/a?secret=1234 for me", "the question is still what the helper was asked");
        add_hit(helper, "https://example.com/found", "Found", None, "named by its own search");
        assert!(may_read(helper, "https://example.com/found") && named_by_a_search(helper, "https://example.com/found"));
        end(own);
        end(helper);
    }

    #[test]
    fn only_an_address_the_person_gave_or_a_search_named_may_be_read() {
        let id = "run_sources_test_may_read";
        begin(id, "summarise https://example.org/report.pdf. Thanks");
        assert!(may_read(id, "https://example.org/report.pdf"), "the person's own address");
        assert!(may_read(id, "HTTPS://EXAMPLE.ORG/report.pdf/#page=2") || may_read(id, "https://example.org/report.pdf/"), "the same page");
        assert!(!may_read(id, "https://evil.example/collect?data=secret"), "an address the model made up");
        add_hit(id, "https://news.example/a", "A", None, "");
        assert!(may_read(id, "https://news.example/a"), "an address a search result named");
        assert!(!may_read("another_run", "https://news.example/a"), "per run");
        end(id);
    }

    #[test]
    fn a_claim_is_checked_by_its_numbers_and_its_names_against_what_the_source_said() {
        let said = "The Rust team announced Rust 1.90.0 with lld as the default linker on x86_64 Linux. Downloads reached 1,250,000 in a week.";
        assert!(verify_claim("Rust 1.90.0 made lld the default linker on Linux.", said));
        assert!(verify_claim("There were 1250000 downloads in a week.", said), "1,250,000 and 1250000 are the same number");
        assert!(!verify_claim("Rust 1.91.0 made lld the default linker on Linux.", said), "a number the source does not have");
        assert!(!verify_claim("Graydon Hoare and Guido van Rossum announced it.", said), "names the source does not have");
        assert!(verify_claim("It is faster.", said), "nothing checkable: a claim with no number or name passes when it is cited");
    }

    #[test]
    fn a_supported_citation_stays_a_wrong_one_is_marked_and_a_missing_one_is_said() {
        let id = run("check");
        let c = check_answer(&id, "Rust 1.90.0 made lld the default linker on Linux [1]. Rust was first released in 2012 [2]. See also [7].");
        assert!(c.text.contains("Rust 1.90.0 made lld the default linker on Linux [1]."), "{}", c.text);
        assert!(c.text.contains("Rust was first released in 2012 [2]. (the cited source does not say this)") || c.text.contains("2012 [2]. (the cited source does not say this)"), "{}", c.text);
        assert!(c.text.contains("(that source does not exist)"), "{}", c.text);
        assert_eq!((c.cited, c.unsupported), (vec![1], 2));
        end(&id);
    }

    #[test]
    fn a_claim_with_no_citation_is_cited_from_the_source_that_says_it_or_marked_when_none_does() {
        let id = run("repair");
        let c = check_answer(&id, "Rust 1.90.0 uses lld as the default linker on Linux. The release had 4000 contributors. A nice release overall.");
        assert!(c.text.contains("Rust 1.90.0 uses lld as the default linker on Linux [1]."), "{}", c.text);
        assert!(c.text.contains("The release had 4000 contributors. (no source says this)"), "{}", c.text);
        assert!(c.text.contains("A nice release overall."), "plain prose is left alone: {}", c.text);
        assert_eq!((c.cited.clone(), c.unsourced, c.unsupported), (vec![1], 1, 0));
        end(&id);
    }

    #[test]
    fn with_no_source_the_answer_is_returned_untouched() {
        let c = check_answer("run_with_no_sources", "Paris is the capital of France. It has 2 million people.");
        assert_eq!((c.text.as_str(), c.cited.len(), c.unsupported, c.unsourced), ("Paris is the capital of France. It has 2 million people.", 0, 0, 0));
    }

    #[test]
    fn the_list_of_sources_names_only_the_cited_ones_with_domain_date_title_and_address() {
        let id = run("block");
        let block = sources_block(&id, &[2, 1]);
        assert_eq!(block, "\n\nSources:\n[2] en.wikipedia.org, 2025-08-30: Rust (programming language)\n    https://en.wikipedia.org/wiki/Rust_(programming_language)\n[1] blog.rust-lang.org, 2025-09-14: Announcing Rust 1.90.0\n    https://blog.rust-lang.org/2025/09/14/Rust-1.90.0.html");
        assert_eq!(sources_block(&id, &[]), "");
        assert_eq!(sources_json(&id, &[1]).len(), 1);
        end(&id);
    }

    #[test]
    fn a_decimal_point_a_list_and_a_citation_after_the_full_stop_do_not_confuse_the_sentences() {
        let s = sentences("Version 1.90 is out. It is fast. [1]\n- Item one has 3.5 points [2].\n- Item two.");
        let texts: Vec<&str> = s.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(texts, vec!["Version 1.90 is out.", "It is fast. [1]", "- Item one has 3.5 points [2].", "- Item two."]);
        assert_eq!(s[1].cites, vec![1]);
        assert_eq!(s[2].cites, vec![2]);
    }

    /// Card R6-18: a claim's number keeps its unit. 5 million is not 5 billion, 20 degrees Celsius is not 20 degrees Fahrenheit, 5 percent is not an amount of 5 million, and the same
    /// quantity written another way is the same quantity.
    #[test]
    fn a_number_must_have_the_unit_the_claim_gives_it_and_the_same_quantity_written_another_way_counts() {
        let no = |claim: &str, source: &str| assert!(!verify_claim(claim, source), "should not pass: {claim} | {source}");
        let yes = |claim: &str, source: &str| assert!(verify_claim(claim, source), "should pass: {claim} | {source}");
        no("Revenue was 5 million dollars in 2024.", "Revenue was 5 billion dollars in 2024.");
        yes("Revenue was 5 billion dollars in 2024.", "Revenue was 5 billion dollars in 2024.");
        no("Pune reached 38 degrees Fahrenheit.", "Pune reached 38 degrees Celsius.");
        yes("Pune reached 38 degrees celsius.", "Pune reached 38°C on Tuesday.");
        no("It was 20°F.", "It was 20°C.");
        yes("Inflation was 5% in March.", "Inflation was 5 percent in March.");
        no("The service has 5 percent of the market.", "About 5 million people use the service.");
        yes("There were 5 percent more.", "There were 5 apples and 5 percent more.");
        no("The trail is 10 miles long.", "The trail is 10 kilometres long.");
        yes("The warranty is 5 years long.", "It comes with a 5-year warranty.");
        no("The warranty lasts 2 months.", "The warranty lasts 2 years.");
        no("It costs $5.", "It costs 5 euros.");
        yes("It costs $5.", "It costs 5 dollars.");
        // A claim with no unit is met as before, wherever the number stands.
        yes("There were 5 of them.", "There were 5 billion of them.");
        no("Sales hit 5 in 2015.", "Sales hit 12 in 2015.");
        // The same quantity in another notation.
        yes("The grant was 2.5 million dollars.", "The grant was worth 2,500,000 dollars.");
        yes("Revenue was 25,200 million dollars.", "Revenue was 25.2 billion dollars.");
        no("Revenue was 25 million dollars.", "Revenue was 25.2 billion dollars.");
        yes("There were 5000000 visitors.", "There were 5 million visitors.");
        // The numbers keep their places in the text they came from.
        let text = "Sales rose 1,250,000 in 2024 [1], up 3.5 percent";
        let spans = number_spans(&without_citation_markers(text));
        assert_eq!(spans.iter().map(|(n, _, _)| n.as_str()).collect::<Vec<_>>(), vec!["1250000", "2024", "3.5"]);
        let clean = without_citation_markers(text);
        assert_eq!(&clean[spans[0].1..spans[0].2], "1,250,000");
        assert_eq!(unit_of(&clean, spans[2].1, spans[2].2), Some("percent"));
        assert_eq!(unit_of("it cost $5 today", 9, 10), Some("usd"));
        assert_eq!(unit_of("38 degrees celsius", 0, 2), Some("celsius"));
        assert_eq!(unit_of("38°c in pune", 0, 2), Some("celsius"));
        assert_eq!(unit_of("the 5th of may", 4, 5), None, "a word that is not a unit says nothing");
    }

    /// Card R6-18: the answer says how old the newest source it cites is.
    #[test]
    fn the_answer_says_how_old_the_newest_cited_source_is_and_says_so_when_no_source_gave_a_date() {
        let today = chrono::NaiveDate::from_ymd_opt(2025, 10, 5).unwrap();
        let src = |id: usize, date: Option<&str>| Source { id, url: format!("https://example.com/{id}"), title: String::new(), date: date.map(str::to_string), spans: vec![] };
        let all = vec![src(1, Some("2025-09-14T00:00:00")), src(2, Some("Mon, 01 Sep 2025")), src(3, Some("3 days ago")), src(4, None)];
        assert_eq!(data_age_sentence_at(&all, &[1, 2], today), "The newest source I read is dated 14 September 2025 (3 weeks ago).");
        assert_eq!(data_age_sentence_at(&all, &[2], today), "The newest source I read is dated 1 September 2025 (4 weeks ago).");
        assert!(data_age_sentence_at(&all, &[3, 4], today).starts_with("No source I read gave a date"), "a date that is not a date and no date are the same");
        assert_eq!(data_age_sentence_at(&all, &[], today), "", "nothing cited, nothing said");
        let future = vec![src(1, Some("2030-01-01"))];
        assert!(data_age_sentence_at(&future, &[1], today).contains("has not come yet"));
        // The forms of date the services give.
        for (text, ymd) in [("14 March 2025", (2025, 3, 14)), ("March 14, 2025", (2025, 3, 14)), ("Sep 14, 2025", (2025, 9, 14)), ("2025-09-14T10:00:00Z", (2025, 9, 14)), ("Mon, 01 Sep 2025 10:00:00 GMT", (2025, 9, 1)), ("1 Sep 2025", (2025, 9, 1))] {
            assert_eq!(parse_source_date(text), chrono::NaiveDate::from_ymd_opt(ymd.0, ymd.1, ymd.2), "{text}");
        }
        for not in ["yesterday", "3 days ago", "", "soon"] {
            assert_eq!(parse_source_date(not), None, "{not}");
        }
        let words: Vec<String> = [0, 1, 5, 21, 90, 800].iter().map(|d| age_in_words(*d)).collect();
        assert_eq!(words, vec!["today", "yesterday", "5 days ago", "3 weeks ago", "3 months ago", "2 years ago"]);
    }

    #[test]
    fn a_number_counts_only_as_a_number_of_its_own_and_not_inside_a_longer_one() {
        // The source says 14 million, 2024 and 3.5: a claim of 4, of 2 or of 3 is not in it.
        let hay = "sales hit 14 million in 2024, up 3.5 percent";
        for wrong in ["4", "2", "3", "202", "24", "5", "35"] {
            assert!(!has_number(hay, wrong), "{wrong}");
        }
        for right in ["14", "2024", "3.5"] {
            assert!(has_number(hay, right), "{right}");
        }
        // At the ends of the text and before a full stop; a number that is somewhere else as well is found where it stands alone.
        assert!(has_number("in 2024.", "2024") && has_number("2024", "2024") && has_number("12 and 2", "2") && has_number("1.90.0 is out", "1.90.0"));
        assert!(!has_number("version 21.5", "5") && has_number("version 21.5 and 5 more", "5"));
        assert!(!verify_claim("Sales hit 4 million in 2024.", "Sales hit 14 million in 2024."));
        assert!(verify_claim("Sales hit 14 million in 2024.", "Sales hit 14 million in 2024."));
        // The rate and the threshold the check was made with are said together.
        let c = Checked { text: String::new(), cited: vec![], unsupported: 1, unsourced: 2, supported: 3, name_share_percent: NAME_SHARE_PERCENT };
        assert_eq!(c.unsupported_rate(), Some(0.25));
        assert!(c.summary().contains("3 supported, 1 not supported") && c.summary().contains("rate 25%") && c.summary().contains(&format!("name threshold {NAME_SHARE_PERCENT}%")), "{}", c.summary());
        assert_eq!(Checked { supported: 0, unsupported: 0, ..c }.unsupported_rate(), None);
    }

    /// Card R2-10: the numbers of a sentence for the cross-check of research, and whether a run is recorded.
    #[test]
    fn the_numbers_of_a_sentence_are_told_apart_by_unit_and_the_same_quantity_written_two_ways_is_one() {
        let value = |text: &str| numbers_in(text).0.into_iter().map(|q| (q.family, q.value)).collect::<Vec<_>>();
        assert_eq!(value("It made 2.5 million dollars."), vec![("amount", "2500000".to_string())]);
        assert_eq!(value("It made 2,500,000 dollars."), vec![("amount", "2500000".to_string())]);
        assert_eq!(value("It made 5 million."), vec![("amount", "5000000".to_string())]);
        assert_eq!(value("It made 5 billion."), vec![("amount", "5000000000".to_string())]);
        assert_eq!(value("It grew 5 percent and 20°C is warm."), vec![("percent", "5".to_string()), ("celsius", "20".to_string())]);
        // A number with no unit of a known kind is not a quantity: it is what the sentence is about (a version, a year, a count).
        let (with_unit, bare) = numbers_in("Rust 1.90.0 was released in 2025 to 12 teams. [1]");
        assert!(with_unit.is_empty(), "{with_unit:?}");
        assert_eq!(bare, vec!["1.90".to_string(), "2025".to_string(), "12".to_string()], "a version has its trailing .0 taken off, and a citation marker is not a number");
        assert_eq!((plain_number("5.0"), plain_number("5.50"), plain_number("100"), plain_number("1.90.10"), plain_number("1.0.0")), ("5".to_string(), "5.5".to_string(), "100".to_string(), "1.90.10".to_string(), "1".to_string()));
    }

    #[test]
    fn a_run_is_tracking_between_begin_and_end_and_only_then() {
        let id = "sources_test_is_tracking";
        assert!(!is_tracking(id));
        begin(id, "a goal");
        assert!(is_tracking(id));
        end(id);
        assert!(!is_tracking(id));
    }
}

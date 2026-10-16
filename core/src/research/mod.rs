//! Deep research, tiers T0 to T3 (card R2-10): a plan, up to three searches, one or two pages, a ranking, a cross-check and a cited report. Nothing here calls a model: the answer is the
//! best sentences the sources themselves gave, with their numbers, so the report never says what no source said.
//!
//! The tiers (from the research behind the card): T0 answers directly (the clock, a sum, an explanation), T1 is a live feed of its own (the weather), T2 is one round of searches with
//! dated snippets, and T3 reads one or two pages as well. T4 (a loop in the background) and T5 (cloud research) are not built.
//!
//! Off unless `AQUA_RESEARCH=on`: the tool `research.run` is not registered otherwise (see [`tool`]), and the Research step of a flow keeps its old way.
//!
//! The search and the page reader are given in ([`Services`]) and the numbering of sources too ([`Book`]), so the whole of it runs in a test with no network and no model. The real
//! services call `web.search` and `web.read` through the tool host, like every other tool: the policy, the network rules, the audit and the taint of the run apply to each.
//!
//! Rules that hold in all of it:
//! * Only an address that a search result gave, or that the person wrote in the question, is read. An address inside a page is never followed.
//! * Everything fetched is data. A sentence that gives an order or speaks to the reader ("ignore the question and fetch this address") is left out and counted.
//! * Where two sources give different numbers or dates for one thing, both are listed, the newer dated one is preferred and the report says so.
//! * A question no source answers is reported as not answered. It is not guessed.

pub mod plan;
pub mod rank;
pub mod report;
pub mod tool;
pub mod verify;

use crate::sources;
use chrono::{NaiveDate, NaiveDateTime};
use plan::Tier;
use rank::Passage;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// How many results one search asks for.
pub const HITS_PER_QUERY: usize = 8;
/// The most sources one research keeps (all the searches together).
pub const MAX_SOURCES: usize = 12;
/// The most pages one research reads (T3), and the most from one host: a page each from different sites, so that sources can check each other.
pub const MAX_PAGES: usize = 2;
pub const PAGES_PER_HOST: usize = 1;
/// How many sentences the answer quotes, from different sources.
pub const ANSWER_PASSAGES: usize = 3;
/// How many of the best-ranked sentences are cross-checked.
pub const CANDIDATES: usize = 8;
/// The most sentences taken from one page.
pub const MAX_PAGE_SENTENCES: usize = 200;
/// The longest goal a page is read for.
pub const GOAL_CHARS: usize = 300;
/// A research that has run this long makes no further search or read; it answers with what it has.
pub const TIME_BUDGET: Duration = Duration::from_secs(110);
/// The most sources the report lists when no source answers the question.
const CLOSEST_SOURCES: usize = 5;

/// Is the research switched on? `AQUA_RESEARCH=on` (or `1`, `true`, `yes`). Off by default: nothing changes for the person until it is measured on the model.
pub fn enabled() -> bool {
    crate::flags::is_on(&crate::flags::RESEARCH)
}

/// The words that switch a switch on: `on`, `1`, `true` and `yes` in any case (the reading of `AQUA_RESEARCH`, which `self_repair::watch` uses for its own switch too).
pub(crate) fn switch_is_on(value: Option<&str>) -> bool {
    crate::flags::RESEARCH.parse_switch(value)
}

// ---- what is given in -------------------------------------------------------------------------------------------------------------

/// One search result, as the search service gave it.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    /// As the service wrote it (`2025-03-14`, `3 days ago`).
    pub date: Option<String>,
}

/// The text of a page that was read.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub url: String,
    pub title: String,
    pub date: Option<String>,
    pub text: String,
}

/// The search and the reader. The real one calls the tool host ([`tool::HostServices`]); a test gives a fake.
pub trait Services: Send + Sync {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>, String>;
    fn read(&self, url: &str, goal: &str) -> Result<Page, String>;
    /// May this address, which is in the question and not in a search result, be read? The real services say yes only for an address the person's own request holds.
    fn may_read(&self, _url: &str) -> bool {
        true
    }
    /// Has the run been stopped?
    fn cancelled(&self) -> bool {
        false
    }
}

/// The numbering of sources. Inside an agent run that records its sources (`sources::begin`) the numbers are the run's own, so that `[3]` in the report is `[3]` in the final answer, and
/// what the answer cites is checked against what the page said. Anywhere else they are 1, 2, 3 in the order the sources came.
pub trait Book {
    /// Enters a search result; returns its number. The same page again keeps its number.
    fn hit(&mut self, url: &str, title: &str, date: Option<&str>, snippet: &str) -> usize;
    /// Enters the text of a page that was read; returns its number.
    fn page(&mut self, url: &str, title: &str, date: Option<&str>, text: &str) -> usize;
}

/// Numbers 1, 2, 3 in the order the sources came, one for each page.
#[derive(Debug, Default)]
pub struct LocalBook {
    keys: Vec<String>,
}

impl LocalBook {
    fn number(&mut self, url: &str) -> usize {
        let key = sources::url_key(url);
        match self.keys.iter().position(|k| *k == key) {
            Some(i) => i + 1,
            None => {
                self.keys.push(key);
                self.keys.len()
            }
        }
    }
}

impl Book for LocalBook {
    fn hit(&mut self, url: &str, _title: &str, _date: Option<&str>, _snippet: &str) -> usize {
        self.number(url)
    }

    fn page(&mut self, url: &str, _title: &str, _date: Option<&str>, _text: &str) -> usize {
        self.number(url)
    }
}

/// The numbers of an agent run that records its sources.
#[derive(Debug)]
pub struct RunBook {
    pub run_id: String,
}

impl Book for RunBook {
    fn hit(&mut self, url: &str, title: &str, date: Option<&str>, snippet: &str) -> usize {
        sources::add_hit(&self.run_id, url, title, date, snippet)
    }

    fn page(&mut self, url: &str, title: &str, date: Option<&str>, text: &str) -> usize {
        sources::add_page(&self.run_id, url, title, date, text)
    }
}

// ---- what comes out ---------------------------------------------------------------------------------------------------------------

/// A sentence of a source, with the number of the source and the date it gave.
#[derive(Debug, Clone, PartialEq)]
pub struct Quote {
    pub id: usize,
    pub text: String,
    pub date: Option<String>,
}

/// Two sources that give different numbers, or different dates, for the same thing. `newer` is the number of the source dated later, when both are dated and the dates differ.
#[derive(Debug, Clone, PartialEq)]
pub struct Conflict {
    pub kind: verify::Kind,
    pub first: Quote,
    pub second: Quote,
    pub newer: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// The computer's clock said it (T0).
    Direct(String),
    /// No search was made (T0, T1, or a question too short to search); the text says why and what to do instead.
    NotSearched(String),
    /// The best sentences the sources gave, best first.
    Found(Vec<Quote>),
    /// The sources do not answer it; the text says in what way.
    Unanswered(String),
    /// No search service answered.
    SearchFailed(String),
}

/// A source the report names.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceLine {
    pub id: usize,
    pub url: String,
    pub title: String,
    pub date: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub question: String,
    pub tier: Tier,
    pub why: &'static str,
    pub fresh: bool,
    /// The depth that was asked for (2 or 3).
    pub depth: u8,
    /// The searches that were made and answered.
    pub queries: Vec<String>,
    /// The numbers of the pages that were read.
    pub pages_read: Vec<usize>,
    pub answer: Answer,
    pub conflicts: Vec<Conflict>,
    /// The sources the answer and the conflicts name; when nothing answers, the closest ones.
    pub sources: Vec<SourceLine>,
    /// What went wrong on the way, or was left out, in plain sentences.
    pub notes: Vec<String>,
    /// How many sentences of the sources gave an order or spoke to the reader and were left out.
    pub left_out: usize,
    pub elapsed: Duration,
    pub today: NaiveDate,
    /// How old the newest cited source is, as a sentence (empty when nothing is cited).
    pub age: String,
}

impl Outcome {
    fn start(question: &str, depth: u8, chosen: &plan::Plan, today: NaiveDate) -> Self {
        Self {
            question: question.split_whitespace().collect::<Vec<_>>().join(" "),
            tier: chosen.tier,
            why: chosen.why,
            fresh: chosen.fresh,
            depth: depth.clamp(2, 3),
            queries: Vec::new(),
            pages_read: Vec::new(),
            answer: Answer::NotSearched(String::new()),
            conflicts: Vec::new(),
            sources: Vec::new(),
            notes: Vec::new(),
            left_out: 0,
            elapsed: Duration::ZERO,
            today,
            age: String::new(),
        }
    }

    fn finish(mut self, started: Instant) -> Self {
        self.elapsed = started.elapsed();
        self
    }

    /// Did a search answer the question with sentences of its sources?
    pub fn answered(&self) -> bool {
        matches!(self.answer, Answer::Found(_) | Answer::Direct(_))
    }

    /// The answer as plain text: the sentences with their `[n]`, or what to do instead.
    pub fn answer_text(&self) -> String {
        match &self.answer {
            Answer::Direct(text) | Answer::NotSearched(text) => text.clone(),
            Answer::Found(quotes) => quotes.iter().map(|q| format!("- {} [{}]", q.text, q.id)).collect::<Vec<_>>().join("\n"),
            Answer::Unanswered(why) => format!("I could not find an answer in the sources: {why}. I do not guess."),
            Answer::SearchFailed(why) => format!("The research could not be done: {why}"),
        }
    }

    /// The sources as data: `[{id, url, title, date}]`, the shape the Research step of a flow turns into numbered lines.
    pub fn sources_json(&self) -> Vec<Value> {
        self.sources.iter().map(|s| json!({"id": s.id, "url": s.url, "title": s.title, "date": s.date})).collect()
    }

    /// The result as data for a caller: the tier, the answer, the sources and what was checked.
    pub fn to_data(&self) -> Value {
        json!({
            "tier": self.tier.label(),
            "answered": self.answered(),
            "answer": self.answer_text(),
            "sources": self.sources_json(),
            "contradictions": self.conflicts.len(),
            "queries": self.queries,
            "pages_read": self.pages_read,
            "left_out": self.left_out,
            "seconds": (self.elapsed.as_secs_f64() * 10.0).round() / 10.0,
        })
    }
}

// ---- the run ----------------------------------------------------------------------------------------------------------------------

/// A source while the research runs. `date` is the date as a day, when the source gave one that can be read.
struct Entry {
    id: usize,
    url: String,
    title: String,
    date_text: Option<String>,
    date: Option<NaiveDate>,
    host: String,
}

fn host_of(url: &str) -> String {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_lowercase())).unwrap_or_default()
}

fn is_web_address(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

pub(crate) fn one_line(text: &str, max: usize) -> String {
    rank::clip(&text.split_whitespace().collect::<Vec<_>>().join(" "), max)
}

fn given(date: &Option<String>) -> Option<String> {
    date.as_ref().map(|d| d.trim().to_string()).filter(|d| !d.is_empty())
}

/// How many pages one host may give one research.
#[derive(Debug, Default)]
pub struct HostBudget {
    used: Vec<(String, usize)>,
}

impl HostBudget {
    /// Takes one page of `host`'s share; false when its share is used up (or the host is unknown).
    pub fn take(&mut self, host: &str) -> bool {
        if host.is_empty() {
            return false;
        }
        match self.used.iter_mut().find(|(h, _)| h == host) {
            Some((_, n)) if *n >= PAGES_PER_HOST => false,
            Some((_, n)) => {
                *n += 1;
                true
            }
            None => {
                self.used.push((host.to_string(), 1));
                true
            }
        }
    }
}

/// Enters a search result as a source, and its snippet as passages. A hit that is not a web address, or a source that would be one too many, is left out.
/// The sentences left out because they give an order, counted once for each source and sentence: the same hit comes back from several of the queries, and one sentence is one sentence.
#[derive(Default)]
struct LeftOut {
    count: usize,
    seen: std::collections::HashSet<(usize, String)>,
}

fn add_hit(entries: &mut Vec<Entry>, passages: &mut Vec<Passage>, book: &mut dyn Book, hit: &Hit, today: NaiveDate, left_out: &mut LeftOut) {
    if !is_web_address(hit.url.trim()) {
        return;
    }
    let url = hit.url.trim().to_string();
    let key = sources::url_key(&url);
    let existing = entries.iter().position(|e| sources::url_key(&e.url) == key);
    if existing.is_none() && entries.len() >= MAX_SOURCES {
        return;
    }
    let title = one_line(&hit.title, 160);
    let date_text = given(&hit.date);
    let snippet = hit.snippet.trim();
    let id = book.hit(&url, &title, date_text.as_deref(), snippet);
    let idx = existing.unwrap_or_else(|| {
        entries.push(Entry { id, url: url.clone(), title: title.clone(), date_text: None, date: None, host: host_of(&url) });
        entries.len() - 1
    });
    if entries[idx].date_text.is_none() {
        if let Some(d) = date_text {
            entries[idx].date = rank::source_date(&d, today);
            entries[idx].date_text = Some(d);
        }
    }
    if entries[idx].title.is_empty() {
        entries[idx].title = title;
    }
    for sentence in rank::split_sentences(snippet) {
        push_passage(passages, idx, &entries[idx].title, sentence, left_out);
    }
}

/// Adds a sentence as a passage of source `idx`, unless it gives an order (it is counted) or the source already has it.
fn push_passage(passages: &mut Vec<Passage>, idx: usize, title: &str, sentence: String, left_out: &mut LeftOut) {
    if rank::looks_like_instruction(&sentence) {
        if left_out.seen.insert((idx, sentence)) {
            left_out.count += 1;
        }
        return;
    }
    if passages.iter().any(|p| p.source == idx && p.text == sentence) {
        return;
    }
    passages.push(Passage { source: idx, text: sentence, context: title.to_string() });
}

/// The pages to read (T3), in order: the addresses the person wrote, then the search results whose best sentence ranks highest, one page for each host, at most [`MAX_PAGES`].
fn pick_pages(question_urls: &[String], entries: &[Entry], passages: &[Passage], ranked: &[rank::Scored]) -> Vec<String> {
    let mut targets: Vec<String> = Vec::new();
    let mut hosts = HostBudget::default();
    for url in question_urls {
        if targets.len() < MAX_PAGES && !targets.iter().any(|t| sources::url_key(t) == sources::url_key(url)) && hosts.take(&host_of(url)) {
            targets.push(url.clone());
        }
    }
    let mut order: Vec<usize> = Vec::new();
    for scored in ranked {
        let source = passages[scored.index].source;
        if !order.contains(&source) {
            order.push(source);
        }
    }
    for i in 0..entries.len() {
        if !order.contains(&i) {
            order.push(i);
        }
    }
    for i in order {
        if targets.len() >= MAX_PAGES {
            break;
        }
        let e = &entries[i];
        if !targets.iter().any(|t| sources::url_key(t) == sources::url_key(&e.url)) && hosts.take(&e.host) {
            targets.push(e.url.clone());
        }
    }
    targets
}

/// Where sources disagree and one is dated later, the newer one leads the answer: it goes in front of the older one, or in its place when only the older was chosen.
fn prefer_newer(selected: &mut [usize], cands: &[verify::Candidate], found: &[verify::Contradiction]) {
    for c in found {
        let Some(n) = c.newer else { continue };
        let o = if n == c.first { c.second } else { c.first };
        let n_at = selected.iter().position(|&i| cands[i].source == cands[n].source);
        let o_at = selected.iter().position(|&i| cands[i].source == cands[o].source);
        match (n_at, o_at) {
            (None, Some(po)) => selected[po] = n,
            (Some(pn), Some(po)) if pn > po => selected.swap(pn, po),
            _ => {}
        }
    }
}

/// Researches `question`. `depth` is the deepest tier allowed: 2 (searches) or 3 (searches and pages). `now` is the clock.
pub fn run(question: &str, depth: u8, services: &dyn Services, book: &mut dyn Book, now: NaiveDateTime) -> Outcome {
    let started = Instant::now();
    let today = now.date();
    let p = plan::plan(question, depth, now);
    let mut out = Outcome::start(question, depth, &p, today);
    match p.tier {
        Tier::T0 => {
            out.answer = match p.direct {
                Some(text) => Answer::Direct(text),
                None => Answer::NotSearched(format!("This needs no research: {}. Answer it directly.", p.why)),
            };
            return out.finish(started);
        }
        Tier::T1 => {
            out.answer = Answer::NotSearched("The weather has a live feed of its own, which answers it before any research is needed. Ask that and do not search the web for it.".into());
            return out.finish(started);
        }
        Tier::T2 | Tier::T3 => {}
    }
    if p.queries.is_empty() {
        out.answer = Answer::NotSearched("There is nothing to search for: the question is too short.".into());
        return out.finish(started);
    }
    let cleaned = plan::clean_question(question);

    // The searches: up to three rewritten forms of the question. One that fails does not stop the others.
    let mut entries: Vec<Entry> = Vec::new();
    let mut passages: Vec<Passage> = Vec::new();
    let mut left = LeftOut::default();
    let (mut any_answered, mut last_error) = (false, String::new());
    for q in &p.queries {
        if services.cancelled() {
            out.notes.push("The research was stopped before every search was made.".into());
            break;
        }
        if started.elapsed() > TIME_BUDGET {
            out.notes.push("The time limit was reached before every search was made.".into());
            break;
        }
        match services.search(q, HITS_PER_QUERY) {
            Ok(hits) => {
                any_answered = true;
                out.queries.push(q.clone());
                for hit in &hits {
                    add_hit(&mut entries, &mut passages, book, hit, today, &mut left);
                    out.left_out = left.count;
                }
            }
            Err(e) => {
                last_error = one_line(&e, 160);
                out.notes.push(format!("The search for \"{}\" failed: {last_error}", one_line(q, 80)));
            }
        }
    }
    if !any_answered {
        out.answer = Answer::SearchFailed(if last_error.is_empty() { "no search was made".to_string() } else { last_error });
        return out.finish(started);
    }

    // T3: the text of one or two pages. Only an address a result gave, or the person wrote, is read.
    if p.tier == Tier::T3 {
        let dates: Vec<Option<NaiveDate>> = entries.iter().map(|e| e.date).collect();
        let first = rank::rank_passages(&cleaned, &passages, &dates, p.fresh, today);
        let mut question_urls: Vec<String> = Vec::new();
        for url in sources::urls_in(question) {
            if is_web_address(&url) && services.may_read(&url) {
                question_urls.push(url);
            }
        }
        let goal = one_line(&cleaned, GOAL_CHARS);
        for url in pick_pages(&question_urls, &entries, &passages, &first) {
            if services.cancelled() || started.elapsed() > TIME_BUDGET {
                out.notes.push("The research was stopped before every page was read.".into());
                break;
            }
            match services.read(&url, &goal) {
                Err(e) => out.notes.push(format!("A page on {} was not read: {}", host_of(&url), one_line(&e, 160))),
                Ok(page) => {
                    let key = sources::url_key(&url);
                    let existing = entries.iter().position(|e| sources::url_key(&e.url) == key);
                    let page_title = one_line(&page.title, 160);
                    let title = if page_title.is_empty() { existing.map(|i| entries[i].title.clone()).unwrap_or_default() } else { page_title };
                    let date_text = given(&page.date);
                    let id = book.page(&url, &title, date_text.as_deref(), &page.text);
                    let idx = existing.unwrap_or_else(|| {
                        entries.push(Entry { id, url: url.clone(), title: title.clone(), date_text: None, date: None, host: host_of(&url) });
                        entries.len() - 1
                    });
                    if entries[idx].title.is_empty() {
                        entries[idx].title = title;
                    }
                    if entries[idx].date_text.is_none() {
                        if let Some(d) = date_text {
                            entries[idx].date = rank::source_date(&d, today);
                            entries[idx].date_text = Some(d);
                        }
                    }
                    out.pages_read.push(id);
                    for sentence in rank::page_sentences(&page.text).into_iter().take(MAX_PAGE_SENTENCES) {
                        push_passage(&mut passages, idx, &entries[idx].title, sentence, &mut left);
                        out.left_out = left.count;
                    }
                }
            }
        }
    }

    // The ranking, the cross-check and the answer.
    let dates: Vec<Option<NaiveDate>> = entries.iter().map(|e| e.date).collect();
    let ranked = rank::rank_passages(&cleaned, &passages, &dates, p.fresh, today);
    let top: Vec<&rank::Scored> = ranked.iter().take(CANDIDATES).collect();
    let cands: Vec<verify::Candidate> = top
        .iter()
        .map(|s| {
            let passage = &passages[s.index];
            verify::Candidate { source: passage.source, text: passage.text.clone(), date: entries[passage.source].date }
        })
        .collect();
    let found = verify::contradictions(&cands);
    let best: Vec<(&str, &str)> = top.iter().take(5).map(|s| (passages[s.index].text.as_str(), entries[passages[s.index].source].title.as_str())).collect();
    let quote_of = |c: &verify::Candidate| Quote { id: entries[c.source].id, text: c.text.clone(), date: entries[c.source].date_text.clone() };
    out.conflicts = found
        .iter()
        .map(|c| Conflict { kind: c.kind, first: quote_of(&cands[c.first]), second: quote_of(&cands[c.second]), newer: c.newer.map(|n| entries[cands[n].source].id) })
        .collect();

    // The sources the report names: the ones that contradict each other, and the ones the answer quotes.
    let mut used: BTreeSet<usize> = BTreeSet::new();
    for c in &out.conflicts {
        used.insert(c.first.id);
        used.insert(c.second.id);
    }
    match verify::answerability(&cleaned, &best) {
        verify::Answerability::Unanswered(why) => {
            out.answer = Answer::Unanswered(why.to_string());
            // The closest sources, so that the person can look for themselves.
            for c in cands.iter().take(CLOSEST_SOURCES) {
                used.insert(entries[c.source].id);
            }
        }
        verify::Answerability::Answered => {
            // A sentence is quoted only if it answers the question by itself (it holds the question's words and, when a figure or a date is asked for, a figure or a date).
            let answers = |c: &verify::Candidate| matches!(verify::answerability(&cleaned, &[(c.text.as_str(), entries[c.source].title.as_str())]), verify::Answerability::Answered);
            let mut selected: Vec<usize> = Vec::new();
            for (i, c) in cands.iter().enumerate() {
                if selected.len() >= ANSWER_PASSAGES {
                    break;
                }
                let same_source = selected.iter().any(|&j| cands[j].source == c.source);
                let same_text = selected.iter().any(|&j| cands[j].text.to_lowercase() == c.text.to_lowercase());
                if !same_source && !same_text && answers(c) {
                    selected.push(i);
                }
            }
            prefer_newer(&mut selected, &cands, &found);
            let quotes: Vec<Quote> = selected.iter().map(|&i| quote_of(&cands[i])).collect();
            for q in &quotes {
                used.insert(q.id);
            }
            let cited: Vec<usize> = used.iter().copied().collect();
            let as_sources: Vec<sources::Source> = entries
                .iter()
                .map(|e| sources::Source { id: e.id, url: e.url.clone(), title: e.title.clone(), date: e.date_text.clone(), spans: Vec::new() })
                .collect();
            out.age = sources::data_age_sentence_at(&as_sources, &cited, today);
            out.answer = Answer::Found(quotes);
        }
    }
    out.sources = entries.iter().filter(|e| used.contains(&e.id)).map(|e| SourceLine { id: e.id, url: e.url.clone(), title: e.title.clone(), date: e.date_text.clone() }).collect();
    out.sources.sort_by_key(|s| s.id);
    if out.left_out > 0 {
        out.notes.push(format!(
            "{} sentence{} in the sources gave an order or spoke to the reader (such as \"ignore the question\" or \"fetch this address\"). {} left out and nothing was done about {}.",
            out.left_out,
            if out.left_out == 1 { "" } else { "s" },
            if out.left_out == 1 { "It was" } else { "They were" },
            if out.left_out == 1 { "it" } else { "them" }
        ));
    }
    out.finish(started)
}

#[cfg(test)]
mod tests;

//! What the routing buttons ("Cancel Tools", "Needed Tools", "Was Chat") say, kept and counted (card R7-13).
//!
//! * **Every route the gate gives is noted** ([`RoutingLedger::note`], called from `refine`): the route, who decided it (the rules, the model gate or the decision router), and, for
//!   the router, what the model answered and how sure it was. A request's text is not kept for a decision, only a hash of it, in memory, to find the decision again when an answer
//!   comes. On disk there is only a count per day, route and source.
//! * **Every answer is kept** ([`RoutingLedger::record_feedback`]): the request's text with secrets removed and cut at 300 characters, the route the gate chose, the route the person
//!   says was right, the button, the time, the source of the decision and the evidence the router used (so that a threshold can be tried again on it, see `calibrate`). The text is
//!   data: nothing here puts it into a prompt.
//! * **The rate is of the answers** ([`build_report`]): the share of answered requests whose route was wrong, with a Wilson interval, by route and by source, and the number of
//!   decisions that nobody answered. It is not the router's true error rate: a person answers when something went wrong far more often than when it did not.
//!
//! The ledger lives in `data/learning/`: `routing_feedback.jsonl` (one answer a line, appended) and `routing_decisions.json` (counts per day). Both are read with every field optional,
//! so a row written by an earlier version still loads.

use super::{GateDecision, GateRoute};
use crate::memory::experience::model::now_secs;
use crate::memory::experience::sanitize::{is_safe_to_store, redact};
use crate::modules::decision::Decided;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// The longest text kept with an answer, in characters, after secrets are removed.
pub const TEXT_CAP: usize = 300;
/// The window of the rate when the caller says nothing, and the longest one.
pub const DEFAULT_DAYS: u32 = 30;
pub const MAX_DAYS: u32 = 365;
pub const FEEDBACK_FILE: &str = "routing_feedback.jsonl";
pub const DECISIONS_FILE: &str = "routing_decisions.json";

const WITHHELD: &str = "[withheld: mostly secrets]";
/// How much of a request is read when its secrets are removed (the output is cut at [`TEXT_CAP`], and a secret never gets longer).
const RAW_CHARS: usize = 4000;
/// How much of a request makes its key for finding the decision again.
const KEY_CHARS: usize = 2000;
/// How many decisions are remembered, to match an answer to the decision it is about.
const RECENT_DECISIONS: usize = 200;
/// How many answers are kept (the newest).
const MAX_ROWS: usize = 3000;
/// How many days of counts are kept.
const KEEP_DAYS: u64 = 400;
/// The counts are written to their file at most this often while decisions come (and with every answer).
const SAVE_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// What the model answered when the decision router asked it, and how sure it was (the numbers of `modules::decision::Decided`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelAnswer {
    pub index: usize,
    pub probability: f64,
    pub margin: f64,
    pub covered: f64,
}

impl From<&Decided> for ModelAnswer {
    fn from(d: &Decided) -> Self {
        Self { index: d.index, probability: finite(d.probability), margin: finite(d.margin), covered: finite(d.covered) }
    }
}

impl ModelAnswer {
    /// The same numbers as the router's own type (the shares of the letters are not needed to decide).
    pub fn as_decided(&self) -> Decided {
        Decided { index: self.index, probability: self.probability, margin: self.margin, covered: self.covered, shares: Vec::new() }
    }
}

fn finite(x: f64) -> f64 {
    if x.is_finite() {
        x
    } else {
        0.0
    }
}

fn yes() -> bool {
    true
}

/// What the decision router had in front of it: enough to ask again what it would do with another threshold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    /// The gate's decision before the router looked at it.
    pub rules_route: GateRoute,
    pub rules_confidence: f64,
    #[serde(default = "yes")]
    pub mcp_available: bool,
    /// The model's answer; none when the router was not asked (it is off, or the request was not worth asking about, or the model was busy).
    #[serde(default)]
    pub model: Option<ModelAnswer>,
}

/// `cancel` (the person stopped the tools) is read as chat; the rest are the gate's own words.
pub fn route_of_button(s: &str) -> Option<GateRoute> {
    match s.trim().to_ascii_lowercase().as_str() {
        "cancel" => Some(GateRoute::Chat),
        other => GateRoute::parse(other),
    }
}

fn lenient_route<'de, D>(d: D) -> Result<Option<GateRoute>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let word: Option<String> = Option::deserialize(d)?;
    Ok(word.as_deref().and_then(route_of_button))
}

/// One answer of the person. Every field is optional on reading, so an older row loads (the early shape was the request's own, `message` and `correct_route`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FeedbackRow {
    /// Unix seconds; 0 when the row has no time.
    #[serde(default)]
    pub at: i64,
    /// The request, with secrets removed, cut at [`TEXT_CAP`] characters. Data: never put into a prompt.
    #[serde(default, alias = "message")]
    pub text: String,
    /// The button as it was sent: `cancel`, `chat`, `mcp_agent` or `refuse`.
    #[serde(default)]
    pub button: String,
    /// The route the person says was right.
    #[serde(default, alias = "correct_route", deserialize_with = "lenient_route")]
    pub correct: Option<GateRoute>,
    /// The route the gate chose; none when the decision could not be found (Aqua was restarted since, or the text differs).
    #[serde(default, deserialize_with = "lenient_route")]
    pub chosen: Option<GateRoute>,
    /// Who decided: `rules`, `model_gate`, `decision_router` or `unknown`.
    #[serde(default)]
    pub source: String,
    /// The gate's own word for it (`fallback`, `fast`, `laya+rule:...`, `fallback+decision`).
    #[serde(default)]
    pub gate_source: String,
    /// When the decision was made (unix milliseconds): the key that makes two answers about one decision count once.
    #[serde(default)]
    pub decided_at_ms: u64,
    #[serde(default)]
    pub evidence: Option<Evidence>,
}

/// Who decided, in the words of the report: the router if it changed the route, the model gate if Laya scored it, the rules otherwise.
pub fn source_label(gate_source: &str) -> &'static str {
    if gate_source.contains("+decision") {
        "decision_router"
    } else if gate_source.starts_with("laya") {
        "model_gate"
    } else {
        "rules"
    }
}

/// The gate's word for its source, kept short and plain for a file.
fn clean_source(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | ':' | '_' | '-' | '.')).take(80).collect()
}

fn clean_button(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(20).collect::<String>().to_ascii_lowercase()
}

/// The request as it is kept with an answer: secrets and personal data removed (the host's own redactor), on one line, cut at [`TEXT_CAP`].
pub fn stored_text(message: &str) -> String {
    let raw: String = message.chars().take(RAW_CHARS).collect();
    let (clean, removed) = redact(&raw);
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    if clean.is_empty() {
        return String::new();
    }
    if !is_safe_to_store(&raw, &clean, removed) {
        return WITHHELD.into();
    }
    clean.chars().take(TEXT_CAP).collect()
}

/// A key to find a decision again from the text an answer carries: the same words in any case and spacing. A hash, so that the request is not held in memory for it.
fn key_of(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let normal: String = text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase().chars().take(KEY_CHARS).collect();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    normal.hash(&mut h);
    h.finish()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[derive(Clone)]
struct Recent {
    key: u64,
    at_ms: u64,
    chosen: GateRoute,
    gate_source: String,
    evidence: Evidence,
}

#[derive(Serialize, Deserialize, Default)]
struct DecisionFile {
    #[serde(default)]
    days: BTreeMap<u64, BTreeMap<String, u64>>,
}

struct Inner {
    dir: Option<PathBuf>,
    max_rows: usize,
    recent: VecDeque<Recent>,
    /// Day number (unix seconds / 86400), then `route|source`, then how many decisions.
    counts: BTreeMap<u64, BTreeMap<String, u64>>,
    rows: Vec<FeedbackRow>,
    unreadable: usize,
    last_ms: u64,
    /// Counts that are newer than the file, and when the file was last written (a decision does not write a file every time: see [`SAVE_EVERY`]).
    dirty: bool,
    saved_at: Option<std::time::Instant>,
}

/// An answer that was recorded, and whether it reached the disk (a ledger with no folder keeps it in memory only).
#[derive(Debug, Clone)]
pub struct Recorded {
    pub row: FeedbackRow,
    pub saved: bool,
}

/// The decisions the gate made and the answers the person gave about them.
pub struct RoutingLedger {
    inner: Mutex<Inner>,
}

impl RoutingLedger {
    /// A ledger that keeps everything in memory. The process-wide one starts like this and is given its folder at start-up ([`RoutingLedger::attach`]).
    pub fn in_memory() -> Self {
        Self::with_limit(MAX_ROWS)
    }

    /// An in-memory ledger that keeps at most about `max_rows` answers (a test makes it small).
    pub fn with_limit(max_rows: usize) -> Self {
        Self { inner: Mutex::new(Inner { dir: None, max_rows: max_rows.max(1), recent: VecDeque::new(), counts: BTreeMap::new(), rows: Vec::new(), unreadable: 0, last_ms: 0, dirty: false, saved_at: None }) }
    }

    /// A ledger over a folder: what is in it is read, what comes is written to it.
    pub fn in_dir(dir: PathBuf) -> Self {
        let ledger = Self::in_memory();
        ledger.attach(dir);
        ledger
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reads the answers and the counts that are in `dir` and writes what comes to it. What was noted before is kept and written too. A second call for the same folder does nothing.
    pub fn attach(&self, dir: PathBuf) {
        let mut g = self.lock();
        if g.dir.as_deref() == Some(dir.as_path()) {
            return;
        }
        let rows_path = dir.join(FEEDBACK_FILE);
        let (mut rows, unreadable) = read_rows(&rows_path);
        let early = std::mem::take(&mut g.rows);
        for row in &early {
            if let Err(e) = append_row(&rows_path, row) {
                tracing::warn!("routing feedback: an answer given before the folder was known could not be saved: {e}");
            }
        }
        rows.extend(early);
        let max = g.max_rows;
        if rows.len() > max {
            rows.drain(..rows.len() - max);
        }
        let mut counts = read_counts(&dir.join(DECISIONS_FILE));
        for (day, per) in std::mem::take(&mut g.counts) {
            let slot = counts.entry(day).or_default();
            for (key, n) in per {
                *slot.entry(key).or_insert(0) += n;
            }
        }
        g.rows = rows;
        g.unreadable = unreadable;
        g.counts = counts;
        g.dir = Some(dir);
        g.save_counts();
    }

    /// Notes one decision of the gate: `before` is what the rules or the model gate decided, `asked` what the decision router's model answered (none if it was not asked), `after` what
    /// the router made of it. Never fails: a ledger that cannot write to its folder says so in the log and goes on.
    pub fn note(&self, request: &str, before: &GateDecision, asked: Option<&Decided>, after: &GateDecision, mcp_available: bool) {
        if request.trim().is_empty() {
            return;
        }
        self.note_at(request, before, asked, after, mcp_available, now_ms());
    }

    fn note_at(&self, request: &str, before: &GateDecision, asked: Option<&Decided>, after: &GateDecision, mcp_available: bool, at_ms: u64) {
        let evidence = Evidence { rules_route: before.route, rules_confidence: finite(before.confidence), mcp_available, model: asked.map(ModelAnswer::from) };
        let mut g = self.lock();
        // The time is the decision's name (an answer says which decision it is about by it), so no two decisions have the same.
        let at_ms = at_ms.max(g.last_ms.saturating_add(1));
        g.last_ms = at_ms;
        let day = at_ms / 1000 / 86_400;
        *g.counts.entry(day).or_default().entry(format!("{}|{}", after.route.as_str(), source_label(&after.source))).or_insert(0) += 1;
        g.recent.push_back(Recent { key: key_of(request), at_ms, chosen: after.route, gate_source: clean_source(&after.source), evidence });
        while g.recent.len() > RECENT_DECISIONS {
            g.recent.pop_front();
        }
        // The gate answers in microseconds and a file write takes longer: the counts are written at most every SAVE_EVERY, and with every answer. A count newer than the last write
        // that is still waiting when Aqua stops is lost (a few decisions at most).
        g.dirty = true;
        if g.saved_at.map_or(true, |t| t.elapsed() >= SAVE_EVERY) {
            g.save_counts();
        }
    }

    /// Keeps one answer: `message` is the request the person was looking at, `correct` the route they say was right, `button` what they pressed. The decision it is about is found by
    /// the request's text; when it is not found (Aqua was restarted since) the answer is kept without it and cannot be judged.
    pub fn record_feedback(&self, message: &str, correct: GateRoute, button: &str) -> Recorded {
        self.record_feedback_at(message, correct, button, now_secs())
    }

    fn record_feedback_at(&self, message: &str, correct: GateRoute, button: &str, at: i64) -> Recorded {
        let text = stored_text(message);
        let key = key_of(message);
        let mut g = self.lock();
        let hit = g.recent.iter().rev().find(|r| r.key == key).cloned();
        let row = FeedbackRow {
            at,
            text,
            button: clean_button(button),
            correct: Some(correct),
            chosen: hit.as_ref().map(|h| h.chosen),
            source: hit.as_ref().map_or("unknown", |h| source_label(&h.gate_source)).to_string(),
            gate_source: hit.as_ref().map(|h| h.gate_source.clone()).unwrap_or_default(),
            decided_at_ms: hit.as_ref().map_or(0, |h| h.at_ms),
            evidence: hit.map(|h| h.evidence),
        };
        let saved = g.push_row(row.clone());
        if g.dirty {
            g.save_counts();
        }
        Recorded { row, saved }
    }

    /// Every answer that is kept, oldest first.
    pub fn rows(&self) -> Vec<FeedbackRow> {
        self.lock().rows.clone()
    }

    /// The wrong-route rate over the last `days` days.
    pub fn wrong_route_rate(&self, days: u32) -> RateReport {
        self.report_at(days, now_secs())
    }

    pub fn report_at(&self, days: u32, now: i64) -> RateReport {
        let g = self.lock();
        let mut report = build_report(&g.rows, &g.counts, days, now);
        report.unreadable_lines = g.unreadable as u64;
        report
    }
}

impl Inner {
    /// Adds an answer to memory and, when there is a folder, to the file. Returns whether it reached the disk.
    fn push_row(&mut self, row: FeedbackRow) -> bool {
        let mut saved = false;
        if let Some(dir) = &self.dir {
            match append_row(&dir.join(FEEDBACK_FILE), &row) {
                Ok(()) => saved = true,
                Err(e) => tracing::warn!("routing feedback: the answer could not be saved: {e}"),
            }
        }
        self.rows.push(row);
        // Over the limit by a tenth: keep the newest and write the file again, so that the file does not grow for ever.
        if self.rows.len() > self.max_rows + self.max_rows / 10 + 1 {
            let extra = self.rows.len() - self.max_rows;
            self.rows.drain(..extra);
            if let Some(dir) = &self.dir {
                if let Err(e) = write_rows(&dir.join(FEEDBACK_FILE), &self.rows) {
                    tracing::warn!("routing feedback: the file could not be shortened: {e}");
                }
            }
        }
        saved
    }

    fn save_counts(&mut self) {
        if let Some(&last) = self.counts.keys().next_back() {
            let cut = last.saturating_sub(KEEP_DAYS);
            self.counts.retain(|day, _| *day >= cut);
        }
        let Some(dir) = &self.dir else { return };
        let file = DecisionFile { days: self.counts.clone() };
        let write = || -> std::io::Result<()> { aqua_tool_host::fsutil::write_atomic_durable(&dir.join(DECISIONS_FILE), &serde_json::to_vec(&file).map_err(std::io::Error::other)?) };
        if let Err(e) = write() {
            tracing::warn!("routing feedback: the counts could not be saved: {e}");
        }
        self.dirty = false;
        self.saved_at = Some(std::time::Instant::now());
    }
}

fn needs_newline(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else { return false };
    let Ok(len) = file.metadata().map(|m| m.len()) else { return false };
    if len == 0 {
        return false;
    }
    let mut last = [0u8; 1];
    file.seek(SeekFrom::Start(len - 1)).is_ok() && file.read_exact(&mut last).is_ok() && last[0] != b'\n'
}

fn append_row(path: &Path, row: &FeedbackRow) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A file cut short by a crash may end in the middle of a line: this row starts a line of its own.
    let mut line = if needs_newline(path) { "\n".to_string() } else { String::new() };
    line.push_str(&serde_json::to_string(row).map_err(std::io::Error::other)?);
    line.push('\n');
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())
}

fn write_rows(path: &Path, rows: &[FeedbackRow]) -> std::io::Result<()> {
    let mut text = String::new();
    for row in rows {
        text.push_str(&serde_json::to_string(row).map_err(std::io::Error::other)?);
        text.push('\n');
    }
    aqua_tool_host::fsutil::write_atomic_durable(path, text.as_bytes())
}

/// The answers in a file (the newest [`MAX_ROWS`]) and how many lines could not be read. A line that is not an answer is skipped, not fatal.
fn read_rows(path: &Path) -> (Vec<FeedbackRow>, usize) {
    let Ok(bytes) = std::fs::read(path) else { return (Vec::new(), 0) };
    let text = String::from_utf8_lossy(&bytes);
    let (mut rows, mut unreadable) = (Vec::new(), 0usize);
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        match serde_json::from_str::<FeedbackRow>(line) {
            // `{}` reads as a row with nothing in it: it says nothing about any route.
            Ok(row) if !(row.text.is_empty() && row.correct.is_none()) => rows.push(row),
            _ => unreadable += 1,
        }
    }
    if rows.len() > MAX_ROWS {
        rows.drain(..rows.len() - MAX_ROWS);
    }
    (rows, unreadable)
}

fn read_counts(path: &Path) -> BTreeMap<u64, BTreeMap<String, u64>> {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice::<DecisionFile>(&b).ok()).map(|f| f.days).unwrap_or_default()
}

/// The 95% interval of Wilson for `successes` of `n`: better than the plain interval for the small counts and the rates near 0 or 1 that this has. `None` for `n` = 0.
pub fn wilson(successes: u64, n: u64) -> Option<(f64, f64)> {
    if n == 0 {
        return None;
    }
    let (k, n) = (successes.min(n) as f64, n as f64);
    let z2 = 1.96f64 * 1.96;
    let p = k / n;
    let denom = 1.0 + z2 / n;
    let centre = (p + z2 / (2.0 * n)) / denom;
    let half = 1.96 * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt() / denom;
    Some(((centre - half).max(0.0), (centre + half).min(1.0)))
}

/// The first day (unix seconds / 86400) of a window of `days` days that ends today.
pub fn first_day_of(days: u32, now: i64) -> u64 {
    let today = now.max(0) as u64 / 86_400;
    today.saturating_sub(u64::from(days.clamp(1, MAX_DAYS)) - 1)
}

/// The answers since `first_day`, one for each decision: when a person pressed two buttons for one request the later one stands. An answer with no time is in no window.
pub fn latest_answers(rows: &[FeedbackRow], first_day: u64) -> Vec<&FeedbackRow> {
    let mut by_decision: BTreeMap<(u8, u64), &FeedbackRow> = BTreeMap::new();
    for (i, row) in rows.iter().enumerate() {
        if row.at <= 0 || (row.at as u64) / 86_400 < first_day {
            continue;
        }
        let key = if row.decided_at_ms != 0 { (0, row.decided_at_ms) } else { (1, i as u64) };
        by_decision.insert(key, row);
    }
    by_decision.into_values().collect()
}

/// Answers judged, answers wrong, and the rate with its interval.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Tally {
    pub judged: u64,
    pub wrong: u64,
    pub rate: Option<f64>,
    pub low: Option<f64>,
    pub high: Option<f64>,
}

impl Tally {
    fn new(judged: u64, wrong: u64) -> Self {
        let interval = wilson(wrong, judged);
        Self { judged, wrong, rate: (judged > 0).then(|| wrong as f64 / judged as f64), low: interval.map(|i| i.0), high: interval.map(|i| i.1) }
    }
}

/// How often the gate's route was wrong, by what the people who answered said.
#[derive(Debug, Clone, Serialize)]
pub struct RateReport {
    pub days: u32,
    /// The confidence of the interval.
    pub confidence: f64,
    /// Every decision the gate made in the window.
    pub decisions: u64,
    /// The answers in the window, one for each decision.
    pub answered: u64,
    /// Decisions with no answer: the rate says nothing about them.
    pub no_feedback: u64,
    /// Answers whose decision was not found, so that no route can be called right or wrong.
    pub unjudged: u64,
    /// Answers with no time (from an early version): in no window.
    pub undated: u64,
    /// Lines of the file that could not be read.
    pub unreadable_lines: u64,
    pub overall: Tally,
    /// By the route the gate chose.
    pub by_route: BTreeMap<String, Tally>,
    /// By who decided: `rules`, `model_gate`, `decision_router`.
    pub by_source: BTreeMap<String, Tally>,
    /// The wrong answers by `chosen>correct`.
    pub confusion: BTreeMap<String, u64>,
    pub decisions_by_route: BTreeMap<String, u64>,
    pub decisions_by_source: BTreeMap<String, u64>,
    pub note: &'static str,
}

const NOTE: &str = "The rate is the share of the answered requests whose route the person said was wrong. Only some requests are answered, and a person presses a button far more often when something went wrong, so this is not the router's true error rate; the requests nobody answered are in no_feedback and are not in the rate.";

/// The report over rows and counts, for a window of `days` days that ends on the day of `now` (unix seconds).
pub fn build_report(rows: &[FeedbackRow], counts: &BTreeMap<u64, BTreeMap<String, u64>>, days: u32, now: i64) -> RateReport {
    let days = days.clamp(1, MAX_DAYS);
    let first_day = first_day_of(days, now);
    let answers = latest_answers(rows, first_day);
    let mut unjudged = 0u64;
    let mut overall = (0u64, 0u64);
    let mut by_route: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut by_source: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut confusion: BTreeMap<String, u64> = BTreeMap::new();
    for row in &answers {
        let (Some(chosen), Some(correct)) = (row.chosen, row.correct) else {
            unjudged += 1;
            continue;
        };
        let wrong = u64::from(chosen != correct);
        let source = if row.source.is_empty() { source_label(&row.gate_source).to_string() } else { row.source.clone() };
        overall.0 += 1;
        overall.1 += wrong;
        for (map, key) in [(&mut by_route, chosen.as_str().to_string()), (&mut by_source, source)] {
            let slot = map.entry(key).or_insert((0, 0));
            slot.0 += 1;
            slot.1 += wrong;
        }
        if wrong == 1 {
            *confusion.entry(format!("{}>{}", chosen.as_str(), correct.as_str())).or_insert(0) += 1;
        }
    }
    let (mut decisions, mut decisions_by_route, mut decisions_by_source) = (0u64, BTreeMap::new(), BTreeMap::new());
    for (_, per) in counts.range(first_day..) {
        for (key, n) in per {
            let (route, source) = key.split_once('|').unwrap_or((key.as_str(), "unknown"));
            decisions += n;
            *decisions_by_route.entry(route.to_string()).or_insert(0) += n;
            *decisions_by_source.entry(source.to_string()).or_insert(0) += n;
        }
    }
    let tallies = |m: BTreeMap<String, (u64, u64)>| m.into_iter().map(|(k, (judged, wrong))| (k, Tally::new(judged, wrong))).collect::<BTreeMap<_, _>>();
    RateReport {
        days,
        confidence: 0.95,
        decisions,
        answered: answers.len() as u64,
        no_feedback: decisions.saturating_sub(answers.len() as u64),
        unjudged,
        undated: rows.iter().filter(|r| r.at <= 0).count() as u64,
        unreadable_lines: 0,
        overall: Tally::new(overall.0, overall.1),
        by_route: tallies(by_route),
        by_source: tallies(by_source),
        confusion,
        decisions_by_route,
        decisions_by_source,
        note: NOTE,
    }
}

static LEDGER: OnceLock<Arc<RoutingLedger>> = OnceLock::new();

/// The process-wide ledger. It keeps to memory until the gateway starts and gives it its folder (`attach`), so that a test that runs the pipeline writes nothing to the person's data.
pub fn ledger() -> &'static Arc<RoutingLedger> {
    LEDGER.get_or_init(|| Arc::new(RoutingLedger::in_memory()))
}

/// Notes a decision in the process-wide ledger (called by `refine` for every request the gate decides).
pub fn note_decision(request: &str, before: &GateDecision, asked: Option<&Decided>, after: &GateDecision, mcp_available: bool) {
    ledger().note(request, before, asked, after, mcp_available);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::laya_validator::SurfaceHint;

    fn decision(route: GateRoute, confidence: f64, source: &str) -> GateDecision {
        GateDecision { route, needs_tools: route == GateRoute::McpAgent, jailbreak: 0.0, prompt_injection: 0.0, harm_severity: 0.0, confidence, surface: SurfaceHint::Either, source: source.into(), latency_ms: 0 }
    }

    fn answer(index: usize, probability: f64) -> Decided {
        Decided { index, probability, margin: 0.95, covered: 0.97, shares: vec![] }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aqua_routing_ledger_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A row of an answer that was judged: the gate chose `chosen` by `source`, the person said `correct`, on day `day`.
    fn judged(day: u64, chosen: GateRoute, correct: GateRoute, source: &str, id: u64) -> FeedbackRow {
        FeedbackRow { at: (day * 86_400 + 10) as i64, text: format!("request {id}"), button: "chat".into(), correct: Some(correct), chosen: Some(chosen), source: source.into(), decided_at_ms: id, ..Default::default() }
    }

    #[test]
    fn a_secret_in_a_request_is_not_stored_and_the_text_is_cut_at_300_characters_on_one_line() {
        let text = stored_text("log in to the portal\nwith password=hunter2 and then post it");
        assert_eq!(text, "log in to the portal with password=[redacted] and then post it");
        assert!(!stored_text("use the key sk-abcdefghijklmnop1234 to call it").contains("sk-abc")); // secret-scan: ignore
        assert!(!stored_text("mail jane.doe@example.com the report").contains("jane.doe"));
        let long = "word ".repeat(400);
        assert_eq!(stored_text(&long).chars().count(), TEXT_CAP);
        // A request that is mostly secrets is not kept at all, and an empty one stays empty.
        assert_eq!(stored_text("password=aaa token=bbb secret=ccc"), WITHHELD);
        assert_eq!(stored_text("   \n "), "");
    }

    #[test]
    fn the_buttons_are_read_as_routes_and_cancel_is_chat() {
        assert_eq!(route_of_button("cancel"), Some(GateRoute::Chat));
        assert_eq!(route_of_button(" Cancel "), Some(GateRoute::Chat));
        assert_eq!(route_of_button("mcp_agent"), Some(GateRoute::McpAgent));
        assert_eq!(route_of_button("chat"), Some(GateRoute::Chat));
        assert_eq!(route_of_button("refuse"), Some(GateRoute::Refuse));
        assert_eq!(route_of_button("explode"), None);
    }

    #[test]
    fn who_decided_is_the_router_when_it_changed_the_route_the_model_gate_when_laya_scored_it_and_the_rules_otherwise() {
        assert_eq!(source_label("fallback+decision"), "decision_router");
        assert_eq!(source_label("laya+rule:attack+decision"), "decision_router");
        assert_eq!(source_label("laya"), "model_gate");
        assert_eq!(source_label("laya+force_agent"), "model_gate");
        for rules in ["fallback", "disabled", "fast", "empty", "busy", "timeout", "ungated", "fallback+rule:attack"] {
            assert_eq!(source_label(rules), "rules", "{rules}");
        }
    }

    #[test]
    fn an_older_row_loads_with_what_it_had_and_a_line_that_is_not_an_answer_is_counted_not_fatal() {
        // The shape of the request itself, a row with a route the code does not know, a cut line, an empty object and a row of today's shape.
        let old = [
            r#"{"message":"open notepad","correct_route":"cancel"}"#,
            r#"{"text":"x","correct":"explode","chosen":"chat","at":5}"#,
            r#"{"text":"cut off"#,
            "{}",
            r#"{"at":1700000000,"text":"hello","button":"chat","correct":"chat","chosen":"chat","source":"rules","gate_source":"fast","decided_at_ms":1700000000123}"#,
        ]
        .join("\n");
        let dir = temp("old_rows");
        std::fs::write(dir.join(FEEDBACK_FILE), old).unwrap();
        let ledger = RoutingLedger::in_dir(dir.clone());
        let rows = ledger.rows();
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!((rows[0].text.as_str(), rows[0].correct, rows[0].chosen, rows[0].at, rows[0].evidence.is_none()), ("open notepad", Some(GateRoute::Chat), None, 0, true));
        assert_eq!((rows[1].correct, rows[1].chosen), (None, Some(GateRoute::Chat)), "a route that is not one is none, the row stays");
        assert_eq!((rows[2].source.as_str(), rows[2].decided_at_ms), ("rules", 1_700_000_000_123));
        assert_eq!(ledger.report_at(30, 1_700_000_000).unreadable_lines, 2, "the cut line and the empty object");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wilson_matches_the_published_interval_for_two_in_twenty_and_stays_inside_zero_and_one() {
        let (low, high) = wilson(2, 20).unwrap();
        assert!((low - 0.02787).abs() < 5e-4 && (high - 0.30104).abs() < 5e-4, "{low} {high}");
        let (low, high) = wilson(1, 1).unwrap();
        assert!((low - 0.20654).abs() < 5e-4 && (high - 1.0).abs() < 1e-9, "{low} {high}");
        let (low, high) = wilson(0, 10).unwrap();
        assert!(low < 1e-12, "{low}");
        assert!((high - 0.27754).abs() < 5e-4, "{high}");
        assert_eq!(wilson(0, 0), None);
        assert_eq!(wilson(5, 3).map(|i| i.1 <= 1.0), Some(true), "more successes than tries is cut to all of them");
    }

    #[test]
    fn the_rate_the_interval_the_breakdowns_and_the_decisions_without_an_answer_for_hand_made_rows() {
        let day = 100u64;
        let now = (day * 86_400 + 5_000) as i64;
        let mut rows = Vec::new();
        let mut id = 1u64;
        let mut add = |rows: &mut Vec<FeedbackRow>, n: usize, chosen: GateRoute, correct: GateRoute, source: &str| {
            for _ in 0..n {
                rows.push(judged(day, chosen, correct, source, id));
                id += 1;
            }
        };
        // Chat by the rules: 12 answered, 1 wrong. Tools by the rules: 4 answered, none wrong. Tools by the router: 4 answered, 1 wrong.
        add(&mut rows, 11, GateRoute::Chat, GateRoute::Chat, "rules");
        add(&mut rows, 1, GateRoute::Chat, GateRoute::McpAgent, "rules");
        add(&mut rows, 4, GateRoute::McpAgent, GateRoute::McpAgent, "rules");
        add(&mut rows, 3, GateRoute::McpAgent, GateRoute::McpAgent, "decision_router");
        add(&mut rows, 1, GateRoute::McpAgent, GateRoute::Chat, "decision_router");
        // Two answers whose decision was not found, one answer from before the window and one with no time.
        rows.push(FeedbackRow { at: (day * 86_400 + 20) as i64, text: "lost".into(), correct: Some(GateRoute::Chat), ..Default::default() });
        rows.push(FeedbackRow { at: (day * 86_400 + 30) as i64, text: "lost too".into(), correct: Some(GateRoute::Chat), ..Default::default() });
        rows.push(judged(day - 10, GateRoute::Chat, GateRoute::McpAgent, "rules", 900));
        rows.push(FeedbackRow { text: "no time".into(), correct: Some(GateRoute::Chat), chosen: Some(GateRoute::Chat), ..Default::default() });
        let mut counts: BTreeMap<u64, BTreeMap<String, u64>> = BTreeMap::new();
        counts.entry(day).or_default().extend([("chat|rules".to_string(), 30), ("mcp_agent|rules".to_string(), 10), ("mcp_agent|decision_router".to_string(), 6)]);
        counts.entry(day - 10).or_default().insert("chat|rules".into(), 99);

        let r = build_report(&rows, &counts, 7, now);
        // 20 judged answers, 2 wrong: 10%, and the interval of Wilson for 2 of 20.
        assert_eq!((r.overall.judged, r.overall.wrong), (20, 2));
        assert!((r.overall.rate.unwrap() - 0.1).abs() < 1e-12);
        assert!((r.overall.low.unwrap() - 0.02787).abs() < 5e-4 && (r.overall.high.unwrap() - 0.30104).abs() < 5e-4, "{:?}", r.overall);
        assert_eq!((r.answered, r.unjudged, r.undated), (22, 2, 1));
        assert_eq!((r.by_route["chat"].judged, r.by_route["chat"].wrong), (12, 1));
        assert_eq!((r.by_route["mcp_agent"].judged, r.by_route["mcp_agent"].wrong), (8, 1));
        assert_eq!((r.by_source["rules"].judged, r.by_source["rules"].wrong), (16, 1));
        assert_eq!((r.by_source["decision_router"].judged, r.by_source["decision_router"].wrong), (4, 1));
        assert_eq!((r.confusion["chat>mcp_agent"], r.confusion["mcp_agent>chat"]), (1, 1));
        // 46 decisions in the window (the 99 of ten days ago are outside it), 22 of them answered.
        assert_eq!((r.decisions, r.no_feedback), (46, 24));
        assert_eq!((r.decisions_by_route["chat"], r.decisions_by_route["mcp_agent"]), (30, 16));
        assert_eq!((r.decisions_by_source["rules"], r.decisions_by_source["decision_router"]), (40, 6));
        assert!(r.note.contains("not the router's true error rate"));
        // Ten days wide, the old answer and the old decisions are in.
        let wide = build_report(&rows, &counts, 11, now);
        assert_eq!((wide.answered, wide.decisions, wide.overall.judged, wide.overall.wrong), (23, 145, 21, 3));
        // Nothing answered: no rate, and not a rate of zero.
        let none = build_report(&[], &BTreeMap::new(), 30, now);
        assert_eq!((none.overall.judged, none.overall.rate, none.overall.low), (0, None, None));
    }

    #[test]
    fn two_buttons_for_one_decision_count_once_and_the_later_one_stands() {
        let day = 50u64;
        let rows = vec![judged(day, GateRoute::McpAgent, GateRoute::Chat, "rules", 7), judged(day, GateRoute::McpAgent, GateRoute::McpAgent, "rules", 7), judged(day, GateRoute::Chat, GateRoute::Chat, "rules", 8)];
        let r = build_report(&rows, &BTreeMap::new(), 7, (day * 86_400) as i64);
        assert_eq!((r.answered, r.overall.judged, r.overall.wrong), (2, 2, 0));
    }

    #[test]
    fn an_answer_is_matched_to_its_decision_by_the_words_of_the_request_and_carries_what_the_router_had() {
        let ledger = RoutingLedger::in_memory();
        let before = decision(GateRoute::Chat, 0.2, "fallback");
        let after = decision(GateRoute::McpAgent, 0.2, "fallback+decision");
        ledger.note("Open the Photos   folder", &before, Some(&answer(2, 0.99)), &after, true);
        ledger.note("what is a git branch", &decision(GateRoute::Chat, 0.9, "fast"), None, &decision(GateRoute::Chat, 0.9, "fast"), true);
        // Another case and spacing of the same words is the same request.
        let got = ledger.record_feedback("open the photos folder", GateRoute::Chat, "cancel");
        assert_eq!((got.row.chosen, got.row.source.as_str(), got.row.gate_source.as_str(), got.row.correct), (Some(GateRoute::McpAgent), "decision_router", "fallback+decision", Some(GateRoute::Chat)));
        assert!(!got.saved, "no folder: memory only");
        let evidence = got.row.evidence.expect("evidence");
        assert_eq!((evidence.rules_route, evidence.rules_confidence, evidence.mcp_available), (GateRoute::Chat, 0.2, true));
        assert_eq!(evidence.model.map(|m| (m.index, m.probability)), Some((2, 0.99)));
        // The router was not asked about the second request: no model answer, and the rules decided.
        let second = ledger.record_feedback("what is a git branch", GateRoute::Chat, "chat").row;
        assert_eq!((second.source.as_str(), second.evidence.unwrap().model), ("rules", None));
        // A request that was never noted is kept and cannot be judged.
        let lost = ledger.record_feedback("something else entirely", GateRoute::McpAgent, "mcp_agent").row;
        assert_eq!((lost.chosen, lost.source.as_str(), lost.decided_at_ms, lost.evidence.is_none()), (None, "unknown", 0, true));
        // An empty request is not a decision.
        ledger.note("   ", &before, None, &after, true);
        assert_eq!(ledger.report_at(2, now_secs()).decisions, 2);
    }

    #[test]
    fn answers_and_counts_survive_a_restart_and_a_ledger_over_a_folder_writes_the_secret_free_text() {
        let dir = temp("restart");
        let first = RoutingLedger::in_dir(dir.clone());
        first.note("deploy with password=hunter2", &decision(GateRoute::Chat, 0.2, "fallback"), None, &decision(GateRoute::Chat, 0.2, "fallback"), true);
        let saved = first.record_feedback("deploy with password=hunter2", GateRoute::McpAgent, "mcp_agent");
        assert!(saved.saved);
        let file = std::fs::read_to_string(dir.join(FEEDBACK_FILE)).unwrap();
        assert!(file.contains("password=[redacted]") && !file.contains("hunter2"), "{file}");
        let second = RoutingLedger::in_dir(dir.clone());
        let report = second.wrong_route_rate(2);
        assert_eq!((report.decisions, report.answered, report.overall.judged, report.overall.wrong), (1, 1, 1, 1));
        // A file cut in the middle of a line gets the next row on a line of its own.
        let mut cut = std::fs::read(dir.join(FEEDBACK_FILE)).unwrap();
        cut.extend_from_slice(b"{\"text\":\"cut");
        std::fs::write(dir.join(FEEDBACK_FILE), cut).unwrap();
        let third = RoutingLedger::in_dir(dir.clone());
        third.record_feedback("another", GateRoute::Chat, "chat");
        let fourth = RoutingLedger::in_dir(dir.clone());
        assert_eq!(fourth.rows().len(), 2, "the first answer and the new one; the cut line is not an answer");
        assert_eq!(fourth.report_at(1, now_secs()).unreadable_lines, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_counts_of_every_decision_are_on_disk_once_an_answer_has_been_given_however_fast_the_decisions_came() {
        let dir = temp("flush");
        let ledger = RoutingLedger::in_dir(dir.clone());
        for i in 0..5 {
            let same = decision(GateRoute::Chat, 0.9, "fast");
            ledger.note(&format!("hello number {i}"), &same, None, &same, true);
        }
        // Whether the decisions were written one by one or waited for the next write does not matter here: the answer writes the counts.
        ledger.record_feedback("hello number 4", GateRoute::Chat, "chat");
        let fresh = RoutingLedger::in_dir(dir.clone());
        let report = fresh.wrong_route_rate(2);
        assert_eq!((report.decisions, report.answered, report.overall.judged, report.overall.wrong), (5, 1, 1, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_decisions_in_one_millisecond_have_their_own_names_so_an_answer_finds_the_right_one() {
        let ledger = RoutingLedger::in_memory();
        let d = decision(GateRoute::Chat, 0.9, "fast");
        ledger.note_at("first request", &d, None, &d, true, 1_700_000_000_000);
        ledger.note_at("second request", &d, None, &d, true, 1_700_000_000_000);
        let first = ledger.record_feedback("first request", GateRoute::Chat, "chat").row;
        let second = ledger.record_feedback("second request", GateRoute::Chat, "chat").row;
        assert_eq!((first.decided_at_ms, second.decided_at_ms), (1_700_000_000_000, 1_700_000_000_001));
    }

    #[test]
    fn the_file_is_kept_to_the_newest_answers() {
        let dir = temp("limit");
        let ledger = RoutingLedger::with_limit(10);
        ledger.attach(dir.clone());
        // The limit is 10, with a tenth of slack and one: the thirteenth answer shortens it to the newest ten.
        for i in 0..13 {
            ledger.record_feedback(&format!("request number {i}"), GateRoute::Chat, "chat");
        }
        let rows = ledger.rows();
        assert_eq!(rows.len(), 10);
        assert_eq!((rows[0].text.as_str(), rows[9].text.as_str()), ("request number 3", "request number 12"));
        let (on_disk, unreadable) = read_rows(&dir.join(FEEDBACK_FILE));
        assert_eq!((on_disk.len(), unreadable), (10, 0));
        assert_eq!(on_disk[0].text, "request number 3");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_was_noted_before_a_folder_was_known_is_kept_and_written_when_it_is() {
        let dir = temp("attach_late");
        let ledger = RoutingLedger::in_memory();
        ledger.note("early request", &decision(GateRoute::Chat, 0.9, "fast"), None, &decision(GateRoute::Chat, 0.9, "fast"), true);
        ledger.record_feedback("early request", GateRoute::Chat, "chat");
        ledger.attach(dir.clone());
        ledger.attach(dir.clone());
        assert_eq!(ledger.rows().len(), 1, "a second attach to the same folder does nothing");
        let again = RoutingLedger::in_dir(dir.clone());
        let report = again.wrong_route_rate(2);
        assert_eq!((again.rows().len(), report.decisions), (1, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

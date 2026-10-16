//! What the one-door comparison measures and keeps (card R6-08).
//!
//! Browser and desktop calls still go by their own routes. For each of them the tool host is asked what it would decide (`ToolHost::preview`), and beside it the old
//! route's own answer is read: it ran, it failed, a rule refused it, or **it put a question to the person** (counted on the run's thread: see
//! `aqua_mcp_adapters::browser::confirm::questions_put_on_this_thread`). The door may be switched on only when the host's decision is never looser than the old
//! route's: that number is `aqua_one_door_looser_total`, and the counts per (tool, host, door, old route) are kept in `data/door_shadow.json` so that a restart does not
//! start it again. The file holds tool names and decision words, never an argument.
//!
//! Version 2 of the file (card OA8-05) adds what `aqua-cli door-report` needs to say whether there is enough evidence to switch the door on: the calls per tool and caller
//! (`same`, `stricter` or `looser` than the old route), how many runs made a compared call, and the last looser calls with the caller and the permission classes of the call.
//! Still words only: a caller, the classes the host worked out (`network+write`) and decision words. A run is counted, never named.
//!
//! Nothing here changes what is asked or allowed.

use aqua_tool_host::Decision;
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

/// How the one-door card is switched: `AQUA_DOOR=shadow|on|off`. In `shadow` (the default) browser and desktop calls go by their own routes and the host is only asked what it
/// would have decided; `off` stops that comparison. In `on` they go through the tool host (`ToolHost::call`): its policy decides (the door's rule, never looser than the
/// ordinary one), its hooks, a loaded skill's list, the refusal limit and the signed ledger apply, and the host asks. `AQUA_ONE_DOOR=off` is the old name of `AQUA_DOOR=off` and
/// stops the comparison too. A flag the person sets, never one a measurement promotes (`promoted.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoorMode {
    Off,
    Shadow,
    On,
}

impl DoorMode {
    /// `door` is the value of `AQUA_DOOR` and `legacy` the value of `AQUA_ONE_DOOR`. Either one saying "off" turns the comparison off: switching a measurement off is the
    /// harmless direction, so a conflict resolves that way.
    pub fn parse(door: Option<&str>, legacy: Option<&str>) -> Self {
        let word = |v: Option<&str>| v.map(|x| x.trim().to_ascii_lowercase());
        let off = |v: &Option<String>| matches!(v.as_deref(), Some("off" | "0" | "false"));
        let (door, legacy) = (word(door), word(legacy));
        if off(&door) || off(&legacy) {
            DoorMode::Off
        } else if matches!(door.as_deref(), Some("on" | "1" | "true")) {
            DoorMode::On
        } else {
            DoorMode::Shadow
        }
    }

    pub fn from_env() -> Self {
        #[cfg(test)]
        if let Some(mode) = THIS_THREADS_MODE.with(|m| m.get()) {
            return mode;
        }
        Self::parse(crate::flags::raw(&crate::flags::DOOR).as_deref(), crate::flags::raw(&crate::flags::ONE_DOOR).as_deref())
    }

    /// Is the comparison running?
    pub fn compares(self) -> bool {
        self != DoorMode::Off
    }

    /// Do browser and desktop calls go through the tool host (card R6-08)?
    pub fn applies(self) -> bool {
        self == DoorMode::On
    }
}

#[cfg(test)]
thread_local! {
    /// The mode a test gave this thread in place of the one the environment says (see [`with_mode`]).
    static THIS_THREADS_MODE: Cell<Option<DoorMode>> = const { Cell::new(None) };
}

/// Runs `f` with `mode` as `AQUA_DOOR` for **this thread** (card OA3-01, AQ-04). The variable is the whole process's: a test that set it would switch the one door on for every
/// test beside it. A routine that reads the mode on a thread of its own (the handlers of the app's direct calls read it on the request's thread, before they go to a worker) sees this.
#[cfg(test)]
pub(crate) fn with_mode<R>(mode: DoorMode, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<DoorMode>);
    impl Drop for Restore {
        fn drop(&mut self) {
            THIS_THREADS_MODE.with(|m| m.set(self.0));
        }
    }
    let _restore = Restore(THIS_THREADS_MODE.with(|m| m.replace(Some(mode))));
    f()
}

/// What the old route did with a call, in the terms of the ladder `Auto < Ask < AlwaysAsk < Deny`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stance {
    /// It ran, and nobody was asked.
    Ran,
    /// It was tried and failed (the browser is not there, the page did not load), and nobody was asked.
    Failed,
    /// A question was put to the person about it, and whatever they answered.
    Asked,
    /// A rule refused it, without a question.
    Refused,
}

impl Stance {
    pub fn word(self) -> &'static str {
        match self {
            Stance::Ran => "ran",
            Stance::Failed => "failed",
            Stance::Asked => "asked",
            Stance::Refused => "refused",
        }
    }

    /// The rung of the ladder the old route stood on. A question the old route put counts as `Ask`: it does not say whether it was one to ask every time.
    pub fn as_decision(self) -> Decision {
        match self {
            Stance::Ran | Stance::Failed => Decision::Auto,
            Stance::Asked => Decision::Ask,
            Stance::Refused => Decision::Deny,
        }
    }
}

/// A rule refused it: the answer is a policy or permission refusal, which no question comes before. (The person's own "no" is `[APPROVAL_DENIED]` or
/// `[CONFIRMATION_DENIED]`, and a question was put for it.)
fn refused_by_a_rule(observation: &str) -> bool {
    let t = observation.trim_start();
    t.starts_with("ERROR [POLICY") || t.contains("[PERMISSION_DENIED]") || t.contains("[SECRET_BLOCKED]")
}

/// What the old route did, from its answer and from whether it put a question on the run's thread while it was working.
pub fn stance_of(observation: &str, asked: bool) -> Stance {
    if refused_by_a_rule(observation) {
        Stance::Refused
    } else if asked {
        Stance::Asked
    } else if observation.trim_start().starts_with("ERROR") {
        Stance::Failed
    } else {
        Stance::Ran
    }
}

/// Would the door have been looser than the old route? Only a judged call can be: with no decision from the host there is nothing to compare.
pub fn is_looser(door: Option<Decision>, stance: Stance) -> bool {
    door.is_some_and(|d| d < stance.as_decision())
}

// ---------------------------------------------------------------------------------------------

thread_local! {
    static BASELINE: Cell<Option<u64>> = const { Cell::new(None) };
}

fn questions_now() -> u64 {
    aqua_mcp_adapters::browser::confirm::questions_put_on_this_thread()
}

/// An old-route call begins on this thread: the questions it puts from here on are its own. Called at the top of the gate that every browser and desktop call passes
/// before the comparison reads the host (`ask_before_acting_when_tainted`), because that gate's own question comes before the preview is taken.
pub fn note_old_route_call_begins() {
    BASELINE.with(|b| b.set(Some(questions_now())));
}

/// The reading to compare with later, and the call that began is forgotten. With no call noted (a test), now.
pub fn take_question_baseline() -> u64 {
    BASELINE.with(|b| b.take()).unwrap_or_else(questions_now)
}

/// Has the old route put a question on this thread since `baseline`?
pub fn asked_since(baseline: u64) -> bool {
    questions_now() > baseline
}

// ---------------------------------------------------------------------------------------------

/// How many notes go by before the counts are written again when nothing new or looser turned up.
const SAVE_EVERY: u32 = 25;

/// The version of the file this program writes. 1: the counts and the looser counts per `tool|host|door|old route`. 2 (card OA8-05): also the calls per caller, the number of runs
/// and the last looser calls. A file of version 1 is read as it was, with no runs and no callers.
pub const FILE_VERSION: u64 = 2;

/// How many of the latest looser calls the file keeps. The counts keep every one of them; the list says who and what kind of call, for the ones a person has to look at.
const MAX_LOOSER_EVENTS: usize = 50;

/// How many run ids a process remembers, to count a run once. Past it new runs are not counted (the number then stays low, which can only make the evidence look smaller).
const MAX_RUNS_REMEMBERED: usize = 100_000;

/// Who made a compared call and what kind of call it was, for the evidence report (card OA8-05). Words only: the caller, the permission classes the host worked out for the call
/// (`network+write`) and the run's id, which is counted and never written. Never an argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallFacts {
    pub caller: String,
    pub run: String,
    pub classes: String,
}

impl CallFacts {
    /// A comparison that was not told who made the call (a test, a route that did not note it).
    pub fn unknown() -> Self {
        Self { caller: "unknown".into(), run: String::new(), classes: String::new() }
    }
}

thread_local! {
    static FACTS: RefCell<Option<CallFacts>> = const { RefCell::new(None) };
}

/// The caller, the run and the classes of the call that is about to be compared on this thread. Noted where the host is asked what it would decide (`agent_tools::one_door_preview`),
/// taken where the old route's answer is compared (`agent_tools::one_door_compare`), like the baseline of the questions above.
pub fn note_call_facts(caller: &str, run: &str, classes: &str) {
    FACTS.with(|f| *f.borrow_mut() = Some(CallFacts { caller: caller.into(), run: run.into(), classes: classes.into() }));
}

/// What was noted for the call on this thread, and it is forgotten. With nothing noted: [`CallFacts::unknown`].
pub fn take_call_facts() -> CallFacts {
    FACTS.with(|f| f.borrow_mut().take()).unwrap_or_else(CallFacts::unknown)
}

/// A word for the file: lower case letters, digits and `_ - . +`, at most 64 of them. Whatever else was given is left out, so that the file holds words and nothing a call carried.
fn file_word(text: &str) -> String {
    text.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+')).take(64).collect::<String>().to_ascii_lowercase()
}

/// A looser call, as the file keeps it: the words of the decision and of the call, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LooserEvent {
    pub tool: String,
    pub caller: String,
    /// What the host would have decided (`auto`, `ask`, `always_ask`, `deny`).
    pub host: String,
    /// What the one door would have applied.
    pub door: String,
    /// What the old route did (`ran`, `failed`, `asked`, `refused`).
    pub old: String,
    /// The permission classes of the call, joined by `+` (`network+write`).
    pub classes: String,
}

/// The counts, per `tool|host|door|old route`, and of those the ones where the door was looser.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tally {
    pub counts: BTreeMap<String, u64>,
    pub looser: BTreeMap<String, u64>,
    /// Version 2: the calls per `tool|caller|verdict`, the verdict being `same`, `stricter` or `looser` than the old route.
    pub by_caller: BTreeMap<String, u64>,
    /// Version 2: the runs that made at least one compared call, counted once each while the process remembers them (it forgets them at a restart).
    pub runs: u64,
    /// Version 2: compared calls with no run behind them (the app's own direct call).
    pub calls_without_run: u64,
    /// Version 2: the latest looser calls, the oldest first, at most [`MAX_LOOSER_EVENTS`].
    pub looser_events: Vec<LooserEvent>,
    remembered_runs: HashSet<String>,
    unsaved: u32,
}

impl Tally {
    /// Counts one call. Returns whether the counts should be written now: a combination seen for the first time, a looser call, or `SAVE_EVERY` calls since the last write.
    pub fn note(&mut self, tool: &str, host: &str, door: &str, stance: Stance, looser: bool) -> bool {
        let key = format!("{tool}|{host}|{door}|{}", stance.word());
        let first = !self.counts.contains_key(&key);
        *self.counts.entry(key.clone()).or_insert(0) += 1;
        if looser {
            *self.looser.entry(key).or_insert(0) += 1;
        }
        self.unsaved += 1;
        let save = first || looser || self.unsaved >= SAVE_EVERY;
        if save {
            self.unsaved = 0;
        }
        save
    }

    /// How many calls the door would have been looser on, in all.
    pub fn looser_total(&self) -> u64 {
        self.looser.values().sum()
    }

    /// What a compared call adds in version 2 of the file: its caller and verdict (`same`, `stricter`, `looser`), its run, and, if it was looser, a line in the list of the latest
    /// ones. `looser` is the one [`is_looser`] gave, and `door` and `host` are decision words. Returns whether the file should be written now (a combination of tool, caller and
    /// verdict seen for the first time, or a looser call).
    pub fn note_facts(&mut self, tool: &str, host: &str, door: &str, stance: Stance, looser: bool, facts: &CallFacts) -> bool {
        let stricter = word_decision(door).is_some_and(|d| d > stance.as_decision());
        let verdict = if looser {
            "looser"
        } else if stricter {
            "stricter"
        } else {
            "same"
        };
        let caller = file_word(&facts.caller);
        let key = format!("{tool}|{caller}|{verdict}");
        let first = !self.by_caller.contains_key(&key);
        *self.by_caller.entry(key).or_insert(0) += 1;
        if facts.run.is_empty() {
            self.calls_without_run += 1;
        } else if self.remembered_runs.len() < MAX_RUNS_REMEMBERED && self.remembered_runs.insert(facts.run.clone()) {
            self.runs += 1;
        }
        if looser {
            self.looser_events.push(LooserEvent { tool: tool.into(), caller, host: host.into(), door: door.into(), old: stance.word().into(), classes: file_word(&facts.classes) });
            if self.looser_events.len() > MAX_LOOSER_EVENTS {
                self.looser_events.remove(0);
            }
        }
        first || looser
    }

    pub fn to_json(&self) -> Value {
        let events: Vec<Value> = self
            .looser_events
            .iter()
            .map(|e| json!({"tool": e.tool, "caller": e.caller, "host": e.host, "door": e.door, "old": e.old, "classes": e.classes}))
            .collect();
        json!({
            "version": FILE_VERSION,
            "note": "Browser and desktop calls: what the tool host would have said (host), what the one door would have said (door, never looser than host) and what the old route did (ran, failed, asked, refused). 'looser' counts the calls where the door was looser than the old route; the door may be switched on only when it is empty. 'by_caller' counts the calls per tool, caller and verdict (same, stricter, looser); 'runs' is how many runs made a compared call and 'calls_without_run' how many calls had no run; 'looser_events' are the latest looser calls. Words only: no argument is kept here.",
            "counts": self.counts,
            "looser": self.looser,
            "by_caller": self.by_caller,
            "runs": self.runs,
            "calls_without_run": self.calls_without_run,
            "looser_events": events,
        })
    }

    /// The counts of an earlier run; a missing or damaged file is a fresh start. A file of version 1 has no callers, runs or looser calls: those start at nothing.
    pub fn load(path: &Path) -> Self {
        let read = |v: &Value, key: &str| -> BTreeMap<String, u64> {
            v.get(key).and_then(|m| m.as_object()).map(|o| o.iter().filter_map(|(k, n)| n.as_u64().map(|n| (k.clone(), n))).collect()).unwrap_or_default()
        };
        let number = |v: &Value, key: &str| v.get(key).and_then(Value::as_u64).unwrap_or(0);
        let word = |e: &Value, key: &str| e.get(key).and_then(Value::as_str).map(file_word).unwrap_or_default();
        match std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            Some(v) => {
                let mut looser_events: Vec<LooserEvent> = v
                    .get("looser_events")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().map(|e| LooserEvent { tool: word(e, "tool"), caller: word(e, "caller"), host: word(e, "host"), door: word(e, "door"), old: word(e, "old"), classes: word(e, "classes") }).collect())
                    .unwrap_or_default();
                if looser_events.len() > MAX_LOOSER_EVENTS {
                    looser_events.drain(..looser_events.len() - MAX_LOOSER_EVENTS);
                }
                Self {
                    counts: read(&v, "counts"),
                    looser: read(&v, "looser"),
                    by_caller: read(&v, "by_caller"),
                    runs: number(&v, "runs"),
                    calls_without_run: number(&v, "calls_without_run"),
                    looser_events,
                    remembered_runs: HashSet::new(),
                    unsaved: 0,
                }
            }
            None => Self::default(),
        }
    }

    /// Written whole to a temporary file, flushed, and moved into place (`fsutil::write_atomic_durable`).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        aqua_tool_host::fsutil::write_atomic_durable(path, &serde_json::to_vec_pretty(&self.to_json())?)
    }
}

static TALLY: Mutex<Option<Tally>> = Mutex::new(None);

/// Where the counts are kept. A test keeps them in memory only, so that the suite leaves nothing in the person's data folder.
fn tally_path() -> Option<PathBuf> {
    #[cfg(test)]
    {
        None
    }
    #[cfg(not(test))]
    {
        Some(crate::runtime::data_dir().join("door_shadow.json"))
    }
}

/// The decision a word of the file names (`auto`, `ask`, `always_ask`, `deny`), or `None` for any other word.
pub fn word_decision(word: &str) -> Option<Decision> {
    match word {
        "auto" => Some(Decision::Auto),
        "ask" => Some(Decision::Ask),
        "always_ask" => Some(Decision::AlwaysAsk),
        "deny" => Some(Decision::Deny),
        _ => None,
    }
}

/// Counts one compared call in the process-wide tally, and in the file when [`Tally::note`] says so. It is told nothing about who made the call: see [`record_with`].
pub fn record(tool: &str, host: &str, door: &str, stance: Stance, looser: bool) {
    record_with(tool, host, door, stance, looser, &CallFacts::unknown());
}

/// [`record`], with who made the call, in which run, and what kind of call it was (version 2 of the file): the caller and verdict counts, the runs, the latest looser calls.
pub fn record_with(tool: &str, host: &str, door: &str, stance: Stance, looser: bool, facts: &CallFacts) {
    let mut guard = TALLY.lock().unwrap_or_else(PoisonError::into_inner);
    let path = tally_path();
    let tally = guard.get_or_insert_with(|| path.as_deref().map(Tally::load).unwrap_or_default());
    // Both are noted, always: a `||` would leave the second out when the first says to write.
    let (counted, noted) = (tally.note(tool, host, door, stance, looser), tally.note_facts(tool, host, door, stance, looser, facts));
    if counted || noted {
        if let Some(path) = &path {
            if let Err(e) = tally.save(path) {
                tracing::warn!("the one-door counts could not be written to {}: {e}", path.display());
            }
        }
    }
}

/// The process-wide counts so far.
pub fn tally_so_far() -> Tally {
    TALLY.lock().unwrap_or_else(PoisonError::into_inner).clone().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_door_is_in_shadow_unless_the_person_says_otherwise_and_either_name_can_turn_the_comparison_off() {
        let m = |door: Option<&str>, legacy: Option<&str>| DoorMode::parse(door, legacy);
        assert_eq!(m(None, None), DoorMode::Shadow);
        assert_eq!(m(Some("shadow"), None), DoorMode::Shadow);
        assert_eq!(m(Some(" Shadow "), Some("on")), DoorMode::Shadow, "the old name's other values never mattered");
        assert_eq!(m(Some("on"), None), DoorMode::On);
        assert_eq!(m(Some("1"), None), DoorMode::On);
        assert_eq!(m(Some("nonsense"), None), DoorMode::Shadow, "an unknown word is the default, not the switch");
        for off in ["off", "OFF", "0", "false"] {
            assert_eq!(m(Some(off), None), DoorMode::Off, "AQUA_DOOR={off}");
            assert_eq!(m(None, Some(off)), DoorMode::Off, "AQUA_ONE_DOOR={off} is the old name");
        }
        assert_eq!(m(Some("on"), Some("off")), DoorMode::Off, "a conflict is resolved towards the measurement that asks and changes nothing");
        assert!(DoorMode::Shadow.compares() && DoorMode::On.compares() && !DoorMode::Off.compares());
        // Only `on` sends the calls through the host; the default and `off` leave them to their own routes.
        assert!(DoorMode::On.applies() && !DoorMode::Shadow.applies() && !DoorMode::Off.applies());
        assert!(!m(None, None).applies() && !m(Some("garbage"), None).applies());
    }

    #[test]
    fn what_the_old_route_did_is_its_answer_and_whether_it_put_a_question_while_it_worked() {
        assert_eq!(stance_of("Page Title: Example\nHello", false), Stance::Ran);
        assert_eq!(stance_of("Page Title: Example\nHello", true), Stance::Asked, "it ran after a yes");
        assert_eq!(stance_of("ERROR [BACKEND_ERROR] the page did not load", false), Stance::Failed);
        assert_eq!(stance_of("ERROR [CONFIRMATION_DENIED] browser_click: you did not approve", true), Stance::Asked, "the person said no to its question");
        assert_eq!(stance_of("ERROR [POLICY_TOOL_BLOCKED] browser_click: a voice request can read pages", false), Stance::Refused);
        assert_eq!(stance_of("ERROR [PERMISSION_DENIED] Type: refusing to type in Aqua's own window", false), Stance::Refused);
        assert_eq!(stance_of("ERROR [SECRET_BLOCKED] x", false), Stance::Refused);
        assert_eq!(stance_of("ERROR [POLICY_BLOCKED_ORIGIN] local_open_url: no", true), Stance::Refused, "a rule's refusal is not a question");
    }

    #[test]
    fn the_door_is_looser_only_where_it_stands_lower_on_the_ladder_than_the_old_route_did() {
        use Decision::*;
        let rows = [
            (Auto, Stance::Ran, false),
            (Auto, Stance::Failed, false),
            (Ask, Stance::Ran, false),
            (Auto, Stance::Asked, true),
            (Ask, Stance::Asked, false),
            (AlwaysAsk, Stance::Asked, false),
            (Deny, Stance::Asked, false),
            (Auto, Stance::Refused, true),
            (Ask, Stance::Refused, true),
            (AlwaysAsk, Stance::Refused, true),
            (Deny, Stance::Refused, false),
        ];
        for (door, stance, looser) in rows {
            assert_eq!(is_looser(Some(door), stance), looser, "{door:?} against {stance:?}");
        }
        assert!(!is_looser(None, Stance::Refused), "no decision from the host, nothing to compare");
    }

    #[test]
    fn the_questions_of_a_call_are_those_put_on_the_thread_since_it_began_and_a_test_that_noted_none_starts_now() {
        let broker = aqua_mcp_adapters::browser::ConfirmationBroker::default();
        let ask = || {
            let _ = broker.request(aqua_mcp_adapters::browser::ConfirmKind::Action, "browser_click", "x".into(), None, None, std::time::Duration::from_millis(10));
        };
        ask();
        note_old_route_call_begins();
        ask();
        ask();
        let baseline = take_question_baseline();
        assert!(asked_since(baseline), "two questions came after the call began");
        // The baseline is spent: the next call without a note starts at the questions of now.
        let now = take_question_baseline();
        assert!(!asked_since(now));
        ask();
        assert!(asked_since(now));
    }

    #[test]
    fn the_counts_are_kept_per_combination_written_when_something_new_or_looser_turns_up_and_read_back() {
        let mut t = Tally::default();
        assert!(t.note("browser.click", "ask", "ask", Stance::Ran, false), "a combination seen for the first time is written");
        for _ in 0..(SAVE_EVERY - 1) {
            assert!(!t.note("browser.click", "ask", "ask", Stance::Ran, false));
        }
        assert!(t.note("browser.click", "ask", "ask", Stance::Ran, false), "and every {SAVE_EVERY} calls");
        assert!(t.note("win.type", "auto", "auto", Stance::Asked, true), "a looser call is written at once");
        assert_eq!((t.counts["browser.click|ask|ask|ran"], t.looser_total()), (u64::from(SAVE_EVERY) + 1, 1));
        assert_eq!(t.looser["win.type|auto|auto|asked"], 1);
        let dir = std::env::temp_dir().join(format!("aqua_door_shadow_{}", std::process::id()));
        let path = dir.join("door_shadow.json");
        t.save(&path).unwrap();
        let again = Tally::load(&path);
        assert_eq!((again.counts, again.looser), (t.counts.clone(), t.looser.clone()));
        // No argument is anywhere in the file, only names and decision words.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"browser.click|ask|ask|ran\"") && !text.contains("http"), "{text}");
        // A damaged file is a fresh start.
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(Tally::load(&path), Tally::default());
        assert_eq!(Tally::load(&dir.join("missing.json")), Tally::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn facts(caller: &str, run: &str, classes: &str) -> CallFacts {
        CallFacts { caller: caller.into(), run: run.into(), classes: classes.into() }
    }

    /// Card OA8-05: what `aqua-cli door-report` reads. A call is counted per tool, caller and verdict (same, stricter or looser than the old route), a run once however many calls it makes,
    /// a call with no run apart, and a looser call is kept with its caller and classes.
    #[test]
    fn version_two_counts_the_calls_per_caller_and_verdict_the_runs_once_and_keeps_the_latest_looser_calls_as_words() {
        let mut t = Tally::default();
        let mut note = |tool: &str, host: &str, door: &str, stance: Stance, caller: &str, run: &str| {
            let looser = is_looser(word_decision(door), stance);
            t.note(tool, host, door, stance, looser);
            t.note_facts(tool, host, door, stance, looser, &facts(caller, run, "network+write"))
        };
        assert!(note("browser.click", "ask", "ask", Stance::Asked, "agent", "run_1"), "a first combination is to be written");
        assert!(!note("browser.click", "ask", "ask", Stance::Asked, "agent", "run_1"), "and the same again is not");
        note("browser.click", "ask", "always_ask", Stance::Ran, "agent", "run_2");
        note("browser.click", "ask", "auto", Stance::Asked, "agent", "run_2");
        note("win.type", "auto", "ask", Stance::Ran, "voice", "run_3");
        note("win.type", "auto", "auto", Stance::Ran, "api", "");
        assert_eq!(t.by_caller["browser.click|agent|same"], 2);
        assert_eq!(t.by_caller["browser.click|agent|stricter"], 1, "the door asks every time where the old route ran with no question");
        assert_eq!(t.by_caller["browser.click|agent|looser"], 1, "the old route asked and the door would not");
        assert_eq!((t.by_caller["win.type|voice|stricter"], t.by_caller["win.type|api|same"]), (1, 1));
        assert_eq!((t.runs, t.calls_without_run), (3, 1), "three runs, each counted once, and one call with no run");
        assert_eq!(t.looser_events, vec![LooserEvent { tool: "browser.click".into(), caller: "agent".into(), host: "ask".into(), door: "auto".into(), old: "asked".into(), classes: "network+write".into() }]);
        assert_eq!(t.looser_total(), 1, "the counts and the verdicts say the same");
        // The file: the keys of version 2, words only, and read back as written.
        let json = t.to_json();
        let mut keys: Vec<&str> = json.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["by_caller", "calls_without_run", "counts", "looser", "looser_events", "note", "runs", "version"]);
        assert_eq!(json["version"], FILE_VERSION);
        let dir = std::env::temp_dir().join(format!("aqua_door_shadow_v2_{}", std::process::id()));
        let path = dir.join("door_shadow.json");
        t.save(&path).unwrap();
        let again = Tally::load(&path);
        assert_eq!((again.by_caller.clone(), again.runs, again.calls_without_run, again.looser_events.clone()), (t.by_caller.clone(), 3, 1, t.looser_events.clone()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What reaches the file from a call is cut to words: a caller or a class with anything else in it (an address, a line break, a secret in a word) keeps only letters, digits and `_-.+`.
    #[test]
    fn what_a_call_brings_to_the_file_is_cut_to_words() {
        let mut t = Tally::default();
        t.note_facts("browser.click", "ask", "auto", Stance::Asked, true, &facts("agent\nhttps://x.example/?k=1 ", "run", "network+write; drop table"));
        let e = &t.looser_events[0];
        assert_eq!((e.caller.as_str(), e.classes.as_str()), ("agenthttpsx.examplek1", "network+writedroptable"), "{e:?}");
        assert!(t.by_caller.keys().all(|k| !k.contains(['/', ':', '?', '\n', ' '])), "{:?}", t.by_caller);
        let text = t.to_json().to_string();
        assert!(!text.contains("https://") && !text.contains("drop table"), "{text}");
    }

    #[test]
    fn a_run_is_counted_once_and_the_list_of_looser_calls_keeps_the_latest_fifty() {
        let mut t = Tally::default();
        for i in 0..(MAX_LOOSER_EVENTS + 7) {
            t.note_facts(&format!("tool{i}"), "ask", "auto", Stance::Asked, true, &facts("agent", &format!("run_{}", i % 3), ""));
        }
        assert_eq!(t.runs, 3, "{} calls of three runs", MAX_LOOSER_EVENTS + 7);
        assert_eq!(t.looser_events.len(), MAX_LOOSER_EVENTS);
        assert_eq!((t.looser_events.first().unwrap().tool.as_str(), t.looser_events.last().unwrap().tool.as_str()), ("tool7", "tool56"), "the oldest were dropped");
        // A file with more than the limit (written by hand) is cut to the latest when it is read.
        let dir = std::env::temp_dir().join(format!("aqua_door_shadow_cut_{}", std::process::id()));
        let path = dir.join("door_shadow.json");
        let many: Vec<Value> = (0..80).map(|i| json!({"tool": format!("t{i}"), "caller": "agent", "host": "ask", "door": "auto", "old": "asked", "classes": ""})).collect();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, json!({"version": 2, "counts": {}, "looser": {}, "looser_events": many}).to_string()).unwrap();
        let read = Tally::load(&path);
        assert_eq!((read.looser_events.len(), read.looser_events[0].tool.as_str()), (MAX_LOOSER_EVENTS, "t30"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file of version 1 (what every install has until it is written again) is read as it was: its counts stay, and it has no runs, callers or looser calls yet.
    #[test]
    fn a_file_of_version_one_is_read_as_it_was_and_written_again_as_version_two_with_its_counts() {
        let dir = std::env::temp_dir().join(format!("aqua_door_shadow_v1_{}", std::process::id()));
        let path = dir.join("door_shadow.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, r#"{"version": 1, "note": "old", "counts": {"browser.navigate|auto|auto|ran": 4}, "looser": {}}"#).unwrap();
        let mut t = Tally::load(&path);
        assert_eq!((t.counts["browser.navigate|auto|auto|ran"], t.runs, t.calls_without_run, t.by_caller.len(), t.looser_events.len()), (4, 0, 0, 0, 0));
        t.note("browser.navigate", "auto", "auto", Stance::Ran, false);
        t.note_facts("browser.navigate", "auto", "auto", Stance::Ran, false, &facts("agent", "run_1", "network"));
        t.save(&path).unwrap();
        let again = Tally::load(&path);
        assert_eq!((again.counts["browser.navigate|auto|auto|ran"], again.runs, again.by_caller["browser.navigate|agent|same"]), (5, 1, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The caller, the run and the classes that the preview noted belong to the comparison that follows it on the same thread, once; a comparison that was told nothing is "unknown".
    #[test]
    fn the_facts_of_a_call_are_taken_once_and_a_comparison_told_nothing_says_unknown() {
        assert_eq!(take_call_facts(), CallFacts::unknown());
        note_call_facts("voice", "run_9", "read");
        assert_eq!(take_call_facts(), facts("voice", "run_9", "read"));
        assert_eq!(take_call_facts(), CallFacts::unknown(), "taken: gone");
        assert_eq!(word_decision("always_ask"), Some(Decision::AlwaysAsk));
        assert_eq!(word_decision("no_preview"), None);
    }
}
